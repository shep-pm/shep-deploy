//! A push that changes none of a target's declared paths: when the poll loop
//! leaves it alone, and when it does not.

use std::fs;

use crate::deploy::test_support::{
    Fixture, RECLAIMED, Shepherd, fixture_before_any_release, install_release,
};
use crate::deploy::{Outcome, deploy, unattended};
use crate::paths::Tree;
use crate::state::State;
use crate::{fixtures, git, swap};

/// The `reload` setting of a sheep that runs from `cmd/voting` alone.
const RELOAD: &str = "reload = ['cmd/voting']\n";

/// A shepherd that holds [`RELOAD`] as the sheep's settings.
fn declaring_reload_paths() -> Shepherd {
    Shepherd::ready().with_settings(RELOAD)
}

/// A target already serving a release, in a repository with `cmd/voting` and
/// `cmd/bot` in it.
fn fixture_declaring_reload_paths() -> Fixture {
    let mut fixture = fixture_before_any_release();
    fixtures::commit(
        fixture.origin.path(),
        &[("cmd/voting/main.go", "1"), ("cmd/bot/main.go", "1")],
        "add the paths",
    );
    git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
        "main",
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    let live = git::remote_head(&fixture.tree.git(), "main").expect("head");
    install_release(&fixture, &live);
    fixture.state.deployed = Some(live);
    fixture
        .state
        .write(&fixture.tree.state_file())
        .expect("write deploy.toml");
    fixture
}

/// Commits `files` on the origin's `main` and returns the new head.
fn push(fixture: &Fixture, files: &[(&str, &str)]) -> String {
    fixtures::commit(fixture.origin.path(), files, "push");
    fixtures::head_of(fixture.origin.path())
}

/// fails if the poll loop still builds and reloads a target for a push that
/// changed nothing the target runs from. The release directory is the proof
/// that no checkout was made either.
#[tokio::test(start_paused = true)]
async fn an_unattended_deploy_skips_a_push_outside_the_paths() {
    let mut fixture = fixture_declaring_reload_paths();
    let live = swap::resolve(&fixture.tree.current()).expect("resolves");
    let before = State::read(&fixture.tree.state_file()).expect("reads");
    let in_memory = fixture.state.clone();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);
    let shepherd = declaring_reload_paths();

    let outcome = unattended(
        &shepherd,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Skipped {
            sha: pushed.clone()
        }
    );
    assert_eq!(shepherd.reload_count(), 0);
    assert_eq!(swap::resolve(&fixture.tree.current()).unwrap(), live);
    assert!(!fixture.tree.release(&pushed).exists(), "no checkout");
    assert_eq!(
        State::read(&fixture.tree.state_file()).expect("reads"),
        before,
        "the record is untouched"
    );
    assert_eq!(fixture.state, in_memory, "and so is the copy in memory");
}

/// fails if `deploy <sheep>` by name skips. An operator asking for a deploy
/// is asking for it now.
#[tokio::test(start_paused = true)]
async fn a_deploy_by_name_does_not_skip() {
    let mut fixture = fixture_declaring_reload_paths();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);

    let outcome = deploy(
        &declaring_reload_paths(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(outcome, Outcome::Deployed { sha: pushed });
}

/// fails if a skipped push stops counting toward the next real one. The diff
/// is against the last sha actually deployed, so the release that finally
/// lands carries the skipped push's change too.
#[tokio::test(start_paused = true)]
async fn a_skipped_push_counts_toward_the_next_real_one() {
    let mut fixture = fixture_declaring_reload_paths();
    let config = fixtures::dog_config();
    let skipped = push(&fixture, &[("cmd/bot/main.go", "2")]);
    let first = unattended(
        &declaring_reload_paths(),
        &fixture.tree,
        &mut fixture.state,
        &config,
    )
    .await
    .expect("completes");
    assert_eq!(
        first,
        Outcome::Skipped {
            sha: skipped.clone()
        }
    );

    let real = push(&fixture, &[("cmd/voting/main.go", "2")]);
    let second = unattended(
        &declaring_reload_paths(),
        &fixture.tree,
        &mut fixture.state,
        &config,
    )
    .await
    .expect("completes");

    assert_eq!(second, Outcome::Deployed { sha: real.clone() });
    let release = fixture.tree.release(&real);
    assert_eq!(
        fs::read_to_string(release.join("cmd/bot/main.go")).expect("reads"),
        "2",
        "the skipped push's change is in the release"
    );
}

/// fails if a push that cannot be judged is skipped. Both failures leave the
/// answer unknown, and unknown deploys as it always did.
#[tokio::test(start_paused = true)]
async fn a_failure_to_decide_deploys_as_before() {
    // The deployed sha is not in the clone, so there is nothing to diff from.
    let mut fixture = fixture_declaring_reload_paths();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);
    fixture.state.deployed = Some(RECLAIMED.to_owned());
    let outcome = unattended(
        &declaring_reload_paths(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    assert_eq!(outcome, Outcome::Deployed { sha: pushed });

    // A pathspec git rejects.
    let mut fixture = fixture_declaring_reload_paths();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);
    let outcome = unattended(
        &Shepherd::ready().with_settings("reload = [':(bogus)cmd']\n"),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    assert_eq!(outcome, Outcome::Deployed { sha: pushed });
}

/// A second sheep, `name`, on the same repository and `$SHEP_HOME` as
/// `fixture`'s, serving the same release.
fn another_sheep(fixture: &Fixture, name: &str) -> (Tree, State) {
    let tree = Tree::for_sheep(fixture.home.path(), name);
    std::fs::create_dir_all(tree.git()).expect("create git dir");
    fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);
    git::fetch(
        &tree.git(),
        &fixture.state.remote,
        "main",
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    fixture
        .state
        .write(&tree.state_file())
        .expect("deploy.toml");
    (tree, fixture.state.clone())
}

/// fails if one sheep's `reload` list is applied to another: two sheep built
/// from one repository each name their own paths, and a push to one's must
/// not reload the other.
#[tokio::test(start_paused = true)]
async fn two_sheep_with_different_reload_lists_deploy_only_the_one_the_push_touches() {
    let mut voting = fixture_declaring_reload_paths();
    let (bot_tree, mut bot_state) = another_sheep(&voting, "bot");
    let pushed = push(&voting, &[("cmd/voting/main.go", "2")]);
    let voting_daemon = Shepherd::ready().with_settings("reload = ['cmd/voting']\n");
    let bot_daemon = Shepherd::ready().with_settings("reload = ['cmd/bot']\n");

    let voting_outcome = unattended(
        &voting_daemon,
        &voting.tree,
        &mut voting.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    let bot_outcome = unattended(
        &bot_daemon,
        &bot_tree,
        &mut bot_state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        voting_outcome,
        Outcome::Deployed {
            sha: pushed.clone()
        }
    );
    assert_eq!(bot_outcome, Outcome::Skipped { sha: pushed });
    assert_eq!(voting_daemon.reload_count(), 1);
    assert_eq!(bot_daemon.reload_count(), 0);
}
