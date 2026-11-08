//! A commit whose build produced the script its sheep last landed: when the
//! poll loop leaves it alone, and when it does not.
//!
//! The compare is against `State::served_script`, the digest the sheep's own
//! last landing recorded, and these fixtures write one the way a landing does.
//! Two of them put the script under the repository's shared build cache, which
//! is where a comparison against the live release goes wrong: that file is one
//! file every release of the remote resolves to, and another sheep's build has
//! already replaced it.

use std::fs;

use crate::deploy::test_support::{
    FIXTURE_TABLE, Fixture, Shepherd, another_sheep, fixture_before_any_release, shared_clone,
};
use crate::deploy::{Outcome, deploy, unattended};
use crate::state::State;
use crate::{fixtures, git, paths, skip, swap};

/// Commits `files` on the origin's `main` and returns the new head.
fn push(fixture: &Fixture, files: &[(&str, &str)]) -> String {
    fixtures::commit(fixture.origin.path(), files, "push");
    fixtures::head_of(fixture.origin.path())
}

/// A sheep that builds `./app` out of `build/input.txt` and has asked for the
/// script compare.
const SCRIPT_BUILD: &str = "command = 'cp build/input.txt app'\nreload_script = true\n";

/// A build whose output is under `target/`, which is where a compiled app's
/// script really lives, and therefore under the repository's shared cache.
const CACHE_BUILD: &str = "command = 'mkdir -p target/release && cp build/input.txt \
                           target/release/app'\nreload_script = true\n";

/// A shepherd whose sheep has `script` as its script and `settings` as its
/// table, and is otherwise the fixture's.
fn a_sheep_that_builds(script: &str, settings: &str) -> Shepherd {
    let mut app = fixtures::gated_app("web", &format!("{FIXTURE_TABLE}{settings}"));
    app.script = script.to_owned();
    Shepherd::ready().with_app(app)
}

/// A target live on its repository's first commit, `current` pointing at that
/// release, and its record carrying the digest a landing would have recorded
/// for a script of `script` bytes.
///
/// Recorded here rather than landed for, because the digest is what these tests
/// are about and a landing is only the thing that normally writes one.
/// [`paths::digest`] is the call the landing makes.
fn serving(daemon: &Shepherd, files: &[(&str, &str)], script: &str) -> Fixture {
    let mut fixture = fixture_before_any_release();
    fixtures::commit(fixture.origin.path(), files, "the app");
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
    fixture.state.served_script = Some(paths::digest(script.as_bytes()));
    fixture
        .state
        .write(&fixture.tree.state_file())
        .expect("deploy.toml");
    fixture
}

/// A target whose live release holds a built `./app` saying `one`.
fn serving_a_built_script(daemon: &Shepherd) -> Fixture {
    serving(daemon, &[("build/input.txt", "one"), ("app", "one")], "one")
}

/// [`serving_a_built_script`] with the live release's `target` linked at the
/// repository's shared build cache, which holds `script` as the binary.
///
/// What a compiled target really is: the live release's script path and the
/// path the next build writes resolve to one file.
fn under_the_shared_cache(daemon: &Shepherd, script: &str) -> Fixture {
    let fixture = serving(daemon, &[("build/input.txt", "one")], script);
    let cache = fixture.tree.cache_target(&fixture.state.remote);
    fs::create_dir_all(cache.join("release")).expect("the shared cache");
    fs::write(cache.join("release/app"), script).expect("the serving binary");
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
    assert_eq!(
        fixture.state.served_script,
        State::read(&fixture.tree.state_file())
            .expect("reads")
            .served_script,
        "and the landing recorded the digest of the binary it landed"
    );
}

/// fails if a tree with nothing recorded starts skipping. A record written
/// before `served_script` existed, or a cutover that has served a release no
/// deploy landed, has no digest, and no digest is not a match.
#[tokio::test]
async fn a_sheep_with_nothing_recorded_deploys_an_unchanged_build() {
    let daemon = a_sheep_that_builds("./app", SCRIPT_BUILD);
    let mut fixture = serving_a_built_script(&daemon);
    fixture.state.served_script = None;
    fixture
        .state
        .write(&fixture.tree.state_file())
        .expect("deploy.toml");
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

/// fails if the compare is taken against the live release's own bytes instead
/// of the recorded ones. A release's `target` is a symlink at the repository's
/// shared build cache, so a script under it is one file every release of that
/// remote resolves to, and this deploy's own build has replaced it by the time
/// a compare that read the live path would run: the new binary against itself,
/// and every commit of a compiled app skipped.
#[tokio::test]
async fn a_script_under_the_shared_cache_is_compared_with_what_was_recorded() {
    // The push the build reads. The cache held "one" and holds "two", and the
    // release that turns it into a different binary deploys.
    let daemon = a_sheep_that_builds("./target/release/app", CACHE_BUILD);
    let mut fixture = under_the_shared_cache(&daemon, "one");
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
    let daemon = a_sheep_that_builds("./target/release/app", CACHE_BUILD);
    let mut fixture = under_the_shared_cache(&daemon, "one");
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

/// fails if a sheep skips a reload because another sheep of its repository has
/// already built the new binary into the shared cache.
///
/// `Tree::cache_target` is one directory for every sheep of one remote, and the
/// poll loop deploys them one after another, so by the time the second is asked
/// the cache holds the first's binary and the second's live release resolves to
/// it through its own `target` link. Its process is still running the old one.
/// Only the digest its own last landing recorded says so, and this is the test
/// that fails when the compare reads the live path instead.
#[tokio::test]
async fn a_sheep_deploys_a_commit_another_sheep_of_its_remote_already_built() {
    let daemon = a_sheep_that_builds("./target/release/app", CACHE_BUILD);
    let mut web = under_the_shared_cache(&daemon, "one");
    let repo = shared_clone(&web);
    // The second sheep of the same remote, on the same first commit, serving
    // from the same shared cache, with the same digest recorded.
    let (api_tree, mut api_state) = another_sheep(&web, &repo, "api", &daemon);
    api_state.served_script = web.state.served_script.clone();
    api_state
        .write(&api_tree.state_file())
        .expect("the second sheep's deploy.toml");

    let pushed = push(&web, &[("build/input.txt", "two")]);

    let first = unattended(&daemon, &web.tree, &mut web.state, &fixtures::dog_config())
        .await
        .expect("the first sheep deploys");
    assert!(matches!(first, Outcome::Deployed { .. }), "{first:?}");
    assert_eq!(
        fs::read_to_string(web.tree.cache_target(&web.state.remote).join("release/app"))
            .expect("reads"),
        "two",
        "and the shared cache now holds the binary it built"
    );

    // The hazard, on disk. The second sheep is still serving its old release,
    // and that release's script path resolves through its `target` link to the
    // binary the first sheep just built. A compare against the live path reads
    // the new binary as itself, and would skip the reload this sheep needs.
    let api_live = swap::resolve(&api_tree.current())
        .expect("reads")
        .expect("a live release");
    assert_eq!(
        fs::read_to_string(api_live.join("target/release/app")).expect("reads"),
        "two",
        "the live release resolves to another sheep's build"
    );

    let second = unattended(&daemon, &api_tree, &mut api_state, &fixtures::dog_config())
        .await
        .expect("the second sheep deploys too");
    assert_eq!(
        second,
        Outcome::Deployed {
            sha: pushed.clone()
        },
        "{second:?}"
    );
    assert_eq!(api_state.deployed.as_deref(), Some(pushed.as_str()));
    assert_eq!(
        api_state.served_script, web.state.served_script,
        "and both records now describe the binary each is running"
    );
    assert_eq!(daemon.reload_count(), 2, "and both were reloaded");
}
