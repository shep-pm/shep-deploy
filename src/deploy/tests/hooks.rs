//! The release and after hooks, through a whole deploy.

use std::fs;

use crate::config::DogConfig;
use crate::deploy::test_support::{
    FLOCKFILE, Fixture, Shepherd, commit_on_origin, fixture_with_previous_release,
};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::flockfile::OVERRIDE;
use crate::hook::Ending;
use crate::state::State;
use crate::{fixtures, swap};

/// Puts `text` in the operator's checkout as a gitignored override, which is
/// how the dog comes to link it into every release.
fn operators_override(fixture: &Fixture, text: &str) {
    fs::write(
        fixture.origin.path().join(".gitignore"),
        format!("{OVERRIDE}\n"),
    )
    .expect("write .gitignore");
    fs::write(fixture.origin.path().join(OVERRIDE), text).expect("write override");
}

/// fails if the hook runs before the build has finished, or after the swap.
///
/// The hook records where `current` points: still the previous release means
/// the swap had not happened, and the marker the build left in the release
/// means the build had.
///
/// Not paused, for the reason on `a_current_that_moved_during_the_build_is_not_swapped_over`.
#[tokio::test]
async fn a_release_hook_runs_after_the_build_and_before_the_swap() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    fs::write(
        fixture.origin.path().join("Flockfile.toml"),
        format!("{FLOCKFILE}\n[dog.deploy.build]\ncommand = 'touch built'\n"),
    )
    .expect("write Flockfile");
    operators_override(
        &fixture,
        "[dog.deploy.release]\ncommand = 'readlink ../../current > current-at-hook.txt; \
         ls built > build-seen-by-hook.txt'\n",
    );
    let second = commit_on_origin(&fixture, "second.txt");

    let outcome = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: second.clone()
        }
    );
    let release = fixture.tree.release(&second);
    let at_hook = fs::read_to_string(release.join("current-at-hook.txt")).expect("hook ran");
    assert_eq!(
        std::path::Path::new(at_hook.trim()),
        fixture.tree.release(&previous)
    );
    assert!(
        release.join("build-seen-by-hook.txt").is_file(),
        "hook ran before the build"
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(release),
        "and the deploy went on to swap"
    );
}

/// fails if a failed migration can reach the swap, or if its sha is retried
/// by the loop. A held sha is the same ending a failed build has.
#[tokio::test]
async fn a_failing_release_hook_never_moves_current_and_holds_the_sha() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    operators_override(&fixture, "[dog.deploy.release]\ncommand = 'exit 4'\n");
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready();

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the hook fails");

    assert!(
        matches!(
            err,
            Error::Hook {
                name: "release",
                ending: Ending::Exited(Some(4))
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
    assert_eq!(daemon.reload_count(), 0, "nothing may be reloaded");
    assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    assert_eq!(fixture.state.failed.as_deref(), Some(second.as_str()));
}

/// fails if a hook runs over a release whose build failed.
#[tokio::test]
async fn a_failing_build_runs_no_release_hook() {
    let mut fixture = fixture_with_previous_release();
    fs::write(
        fixture.origin.path().join("Flockfile.toml"),
        format!("{FLOCKFILE}\n[dog.deploy.build]\ncommand = 'exit 3'\n"),
    )
    .expect("write Flockfile");
    operators_override(
        &fixture,
        "[dog.deploy.release]\ncommand = 'touch hook-ran'\n",
    );
    let second = commit_on_origin(&fixture, "second.txt");

    let err = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the build fails");

    assert!(matches!(err, Error::Build { status: Some(3) }), "{err:?}");
    assert!(
        !fixture.tree.release(&second).join("hook-ran").exists(),
        "the hook ran after a failed build"
    );
}

/// fails if the after hook runs before the release is recorded, or does not
/// see the three variables. The hook copies `deploy.toml` as it finds it, so
/// a hook that ran before `record_landing` would copy the previous sha.
///
/// Not paused, for the reason on `a_current_that_moved_during_the_build_is_not_swapped_over`.
#[tokio::test]
async fn an_after_hook_runs_once_the_release_is_verified_and_recorded() {
    let mut fixture = fixture_with_previous_release();
    operators_override(
        &fixture,
        "[dog.deploy.after]\ncommand = 'cp ../../deploy.toml record-at-hook.toml; \
         readlink ../../current > current-at-hook.txt; \
         printenv SHEP_DEPLOY_SHEEP > sheep.txt; printenv SHEP_DEPLOY_SHA > sha.txt; \
         printenv SHEP_DEPLOY_RELEASE > release.txt; echo ran >> runs.txt'\n",
    );
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready();

    let outcome = deploy(
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
            sha: second.clone()
        }
    );
    let release = fixture.tree.release(&second);
    let record = State::read(&release.join("record-at-hook.toml")).expect("the hook ran");
    assert_eq!(record.deployed.as_deref(), Some(second.as_str()));
    let at_hook = fs::read_to_string(release.join("current-at-hook.txt")).expect("current");
    assert_eq!(std::path::Path::new(at_hook.trim()), release);
    let seen = |name: &str| fs::read_to_string(release.join(name)).expect(name);
    assert_eq!(seen("sheep.txt").trim(), fixture.tree.sheep());
    assert_eq!(seen("sha.txt").trim(), second);
    assert_eq!(
        std::path::Path::new(seen("release.txt").trim())
            .canonicalize()
            .expect("exists"),
        release.canonicalize().expect("canonical")
    );
    assert_eq!(seen("runs.txt"), "ran\n", "once per deploy");
    assert_eq!(daemon.reload_count(), 1);
}

/// fails if the after hook runs over a release that was rolled back, which
/// would purge caches for code that is not serving.
#[tokio::test(start_paused = true)]
async fn an_after_hook_does_not_run_when_the_release_is_rolled_back() {
    let mut fixture = fixture_with_previous_release();
    operators_override(
        &fixture,
        "[dog.deploy.after]\ncommand = 'touch after-ran'\n",
    );
    let second = commit_on_origin(&fixture, "second.txt");

    let outcome = deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
    assert!(
        !fixture.tree.release(&second).join("after-ran").exists(),
        "the after hook ran over a release that was rolled back"
    );
}

/// fails if a failed purge can undo a release that is live and verified, or
/// hold its sha so the loop treats a serving release as a failed one.
#[tokio::test]
async fn a_failing_after_hook_still_reports_deployed_and_writes_no_hold() {
    let mut fixture = fixture_with_previous_release();
    operators_override(&fixture, "[dog.deploy.after]\ncommand = 'exit 5'\n");
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready();

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("a failing after hook is not a failed deploy");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: second.clone()
        }
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&second))
    );
    assert_eq!(daemon.reload_count(), 1, "nothing may be rolled back");
    assert_eq!(fixture.state.deployed.as_deref(), Some(second.as_str()));
    assert_eq!(fixture.state.failed, None);
    let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
    assert_eq!(written.deployed.as_deref(), Some(second.as_str()));
    assert_eq!(written.failed, None);
}

/// fails if an after hook that never finishes holds the tree lock forever, or
/// turns the deploy it follows into a failure.
///
/// Not paused, for the reason on `a_hook_that_never_finishes_is_abandoned`
/// in `hook`.
#[tokio::test]
async fn an_after_hook_that_never_finishes_is_abandoned_and_the_deploy_is_still_deployed() {
    let mut fixture = fixture_with_previous_release();
    operators_override(
        &fixture,
        "[dog.deploy.after]\ncommand = 'sleep 600 & echo $! > descendant.pid; wait'\n",
    );
    let second = commit_on_origin(&fixture, "second.txt");
    let config = DogConfig {
        build_timeout: std::time::Duration::from_secs(2),
        ..fixtures::dog_config()
    };

    let outcome = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &config,
    )
    .await
    .expect("a timed-out after hook is not a failed deploy");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: second.clone()
        }
    );
    assert_eq!(fixture.state.failed, None);
}
