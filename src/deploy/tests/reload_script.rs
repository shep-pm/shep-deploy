//! A commit whose build produced the script its sheep is already serving: when
//! the poll loop leaves it alone, and when it does not.

use std::fs;

use crate::deploy::test_support::{FIXTURE_TABLE, Fixture, Shepherd, fixture_before_any_release};
use crate::deploy::{Outcome, deploy, unattended};
use crate::state::State;
use crate::{fixtures, git, skip, swap};

/// Commits `files` on the origin's `main` and returns the new head.
fn push(fixture: &Fixture, files: &[(&str, &str)]) -> String {
    fixtures::commit(fixture.origin.path(), files, "push");
    fixtures::head_of(fixture.origin.path())
}

/// A sheep that builds `./app` out of `build/input.txt` and has asked for the
/// script compare.
const SCRIPT_BUILD: &str = "command = 'cp build/input.txt app'\nreload_script = true\n";

/// A shepherd whose sheep has `script` as its script and `settings` as its
/// table, and is otherwise the fixture's.
fn a_sheep_that_builds(script: &str, settings: &str) -> Shepherd {
    let mut app = fixtures::gated_app("web", &format!("{FIXTURE_TABLE}{settings}"));
    app.script = script.to_owned();
    Shepherd::ready().with_app(app)
}

/// A target whose live release holds a built `./app`, in a repository whose
/// `build/input.txt` says `one`.
///
/// The built script is committed rather than built here. The bytes a live
/// release holds at its `script` path are what the compare is against, and a
/// commit puts the same bytes there without a build the test is not about.
fn serving_a_built_script(daemon: &Shepherd) -> Fixture {
    let mut fixture = fixture_before_any_release();
    fixtures::commit(
        fixture.origin.path(),
        &[("build/input.txt", "one"), ("app", "one")],
        "the app",
    );
    git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
        "main",
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    let live = git::remote_head(&fixture.tree.git(), "main").expect("head");
    let release = fixture.release_of(daemon, &live);
    git::worktree_add(&fixture.tree.git(), &release, &live).expect("worktree");
    swap::point_at(&fixture.tree.current(), &release).expect("current");
    fixture.state.deployed = Some(live);
    fixture
        .state
        .write(&fixture.tree.state_file())
        .expect("deploy.toml");
    fixture
}

/// [`serving_a_built_script`] with the live release's `target` linked at the
/// repository's shared build cache, which holds `serving` as the binary.
///
/// What a compiled target really is: a release's `target` is a link, so the
/// script the live release runs and the one the next build writes are one file.
fn serving_under_the_shared_cache(daemon: &Shepherd, serving: &str) -> Fixture {
    let fixture = serving_a_built_script(daemon);
    let cache = fixture.tree.cache_target(&fixture.state.remote);
    fs::create_dir_all(cache.join("release")).expect("the shared cache");
    fs::write(cache.join("release/app"), serving).expect("the serving binary");
    let live = swap::resolve(&fixture.tree.current())
        .expect("reads")
        .expect("a live release");
    std::os::unix::fs::symlink(&cache, live.join("target"))
        .expect("the live release's target link");
    fixture
}

/// fails if a commit whose build produced the very script already serving is
/// swapped in and reloaded anyway. The build is the expensive half and it has
/// already been paid for; the reload is what this setting exists to skip.
// Not paused, for the reason on `attempt_behaves_as_before_after_the_split`:
// this runs a real build command.
#[tokio::test]
async fn an_unattended_deploy_skips_a_commit_whose_build_produced_the_same_script() {
    let daemon = a_sheep_that_builds("./app", SCRIPT_BUILD);
    let mut fixture = serving_a_built_script(&daemon);
    let live = swap::resolve(&fixture.tree.current()).expect("a live release");
    let before = State::read(&fixture.tree.state_file()).expect("reads");
    let in_memory = fixture.state.clone();
    let pushed = push(&fixture, &[("notes.txt", "2")]);

    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Skipped {
            sha: pushed.clone(),
            reason: skip::Reason::UnchangedScript
        }
    );
    assert_eq!(daemon.reload_count(), 0, "the sheep was not reloaded");
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        live,
        "current is unswapped"
    );
    assert_eq!(
        fs::read_to_string(fixture.release_of(&daemon, &pushed).join("app")).expect("reads"),
        "one",
        "the build ran, and produced what is already serving"
    );
    assert_eq!(
        State::read(&fixture.tree.state_file()).expect("reads"),
        before,
        "the record is untouched"
    );
    assert_eq!(fixture.state, in_memory, "and so is the copy in memory");
}

/// fails if the compare skips a build that produced a different script. A
/// sheep that recompiles its app on every commit would deploy nothing at all
/// if an unchanged build were the only thing this setting could tell.
#[tokio::test]
async fn a_commit_whose_build_produced_a_different_script_deploys() {
    let daemon = a_sheep_that_builds("./app", SCRIPT_BUILD);
    let mut fixture = serving_a_built_script(&daemon);
    let pushed = push(&fixture, &[("build/input.txt", "two")]);

    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: pushed.clone()
        }
    );
    assert_eq!(daemon.reload_count(), 1);
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.release_of(&daemon, &pushed))
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(pushed.as_str()));
}

/// fails if a hand deploy skips on the script. `reload_script` says the poll
/// loop may leave a commit alone, and an operator typing `deploy <sheep>` is
/// asking for the reload now.
#[tokio::test]
async fn a_deploy_by_name_never_skips_on_the_script() {
    let daemon = a_sheep_that_builds("./app", SCRIPT_BUILD);
    let mut fixture = serving_a_built_script(&daemon);
    let pushed = push(&fixture, &[("notes.txt", "2")]);

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(outcome, Outcome::Deployed { sha: pushed });
    assert_eq!(daemon.reload_count(), 1);
}

/// fails if the compare runs without being asked for. An interpreted app's
/// `script` misses the files it imports, so a dog that compared bytes on its
/// own would skip reloads that its app needs.
#[tokio::test]
async fn a_sheep_that_did_not_ask_for_the_compare_deploys_an_unchanged_build() {
    let daemon = a_sheep_that_builds("./app", "command = 'cp build/input.txt app'\n");
    let mut fixture = serving_a_built_script(&daemon);
    let pushed = push(&fixture, &[("notes.txt", "2")]);

    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(outcome, Outcome::Deployed { sha: pushed });
    assert_eq!(daemon.reload_count(), 1);
}

/// fails if a skip runs the release hook. A migration is what operators put
/// there, and running one for a release that is never swapped in changes the
/// database the live release is serving from.
#[tokio::test]
async fn a_skip_on_the_script_runs_no_release_hook() {
    let daemon = a_sheep_that_builds(
        "./app",
        &format!("{SCRIPT_BUILD}[release]\ncommand = 'touch hooked'\n"),
    );
    let mut fixture = serving_a_built_script(&daemon);
    let pushed = push(&fixture, &[("notes.txt", "2")]);

    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert!(matches!(outcome, Outcome::Skipped { .. }), "{outcome:?}");
    let release = fixture.release_of(&daemon, &pushed);
    assert!(release.join("app").is_file(), "the build ran");
    assert!(!release.join("hooked").exists(), "the hook did not");
}

/// fails if the live script is read after the build instead of before it. A
/// release's `target` is a symlink at the repository's shared build cache, so
/// a script under it is one file every release of that remote resolves to:
/// read after the build, the compare is the new binary against itself, and
/// every commit of a compiled app is skipped.
#[tokio::test]
async fn a_script_under_the_shared_cache_is_compared_with_what_was_serving() {
    let build = "command = 'mkdir -p target/release && cp build/input.txt target/release/app'\n\
                 reload_script = true\n";

    // The push the build reads. The cache held "one" and holds "two", and the
    // release that turns it into a different binary deploys.
    let daemon = a_sheep_that_builds("./target/release/app", build);
    let mut fixture = serving_under_the_shared_cache(&daemon, "one");
    let pushed = push(&fixture, &[("build/input.txt", "two")]);
    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    assert_eq!(outcome, Outcome::Deployed { sha: pushed }, "{outcome:?}");

    // The push it does not read, over one more commit's worth of cache.
    let daemon = a_sheep_that_builds("./target/release/app", build);
    let mut fixture = serving_under_the_shared_cache(&daemon, "one");
    let pushed = push(&fixture, &[("notes.txt", "2")]);
    let outcome = unattended(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Skipped {
            sha: pushed,
            reason: skip::Reason::UnchangedScript
        }
    );
    assert_eq!(daemon.reload_count(), 0);
}
