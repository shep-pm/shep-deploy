//! A push that changes none of a target's declared paths: when the poll loop
//! leaves it alone, and when it does not.

use std::fs;

use crate::deploy::test_support::{
    FLOCKFILE, Fixture, RECLAIMED, Shepherd, fixture_before_any_release, install_release,
};
use crate::deploy::{Outcome, deploy, unattended};
use crate::state::State;
use crate::{fixtures, git, swap};

const RELOAD: &str = "[dog.deploy.reload]\npaths = [\"cmd/voting\"]\n";

/// A target already serving a release whose Flockfile declares
/// `cmd/voting` as the only path it runs from.
fn fixture_declaring_reload_paths() -> Fixture {
    let mut fixture = fixture_before_any_release();
    fixtures::commit(
        fixture.origin.path(),
        &[
            ("Flockfile.toml", &format!("{FLOCKFILE}\n{RELOAD}")),
            ("cmd/voting/main.go", "1"),
            ("cmd/bot/main.go", "1"),
        ],
        "declare reload paths",
    );
    git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
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
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);
    let shepherd = Shepherd::ready();

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
    assert_eq!(fixture.state, before, "and so is the copy in memory");
}

/// fails if `deploy <sheep>` by name skips. An operator asking for a deploy
/// is asking for it now.
#[tokio::test(start_paused = true)]
async fn a_deploy_by_name_does_not_skip() {
    let mut fixture = fixture_declaring_reload_paths();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);

    let outcome = deploy(
        &Shepherd::ready(),
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
        &Shepherd::ready(),
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
        &Shepherd::ready(),
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
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    assert_eq!(outcome, Outcome::Deployed { sha: pushed });

    // The live release has no Flockfile to read the paths from.
    let mut fixture = fixture_declaring_reload_paths();
    let pushed = push(&fixture, &[("cmd/bot/main.go", "2")]);
    let live = swap::resolve(&fixture.tree.current())
        .expect("resolves")
        .expect("a live release");
    fs::remove_file(live.join("Flockfile.toml")).expect("remove");
    let outcome = unattended(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");
    assert_eq!(outcome, Outcome::Deployed { sha: pushed });
}
