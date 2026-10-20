//! A target moved to another branch: what a deploy in flight does not undo,
//! and what the next one builds.

use crate::deploy::test_support::{Shepherd, commit_on_origin, fixture_with_previous_release};
use crate::deploy::{Outcome, deploy, record_landing};
use crate::fixtures;
use crate::retarget::set_branch;
use crate::state::State;

/// fails if a deploy's final write of the record puts the old branch back.
/// The deploy holds its `State` from its first read to its last write, a
/// build's worth of time, and `--branch` lands in the middle of it. The
/// move has to survive, and the deploy's own fact has to be recorded by the
/// same write.
#[test]
fn a_branch_set_while_a_deploy_writes_the_record_loses_nothing() {
    let fixture = fixture_with_previous_release();
    // The copy a deploy holds from its first read: still on `main`.
    let mut in_flight = fixture.state.clone();

    // The operator, mid-deploy.
    set_branch(&fixture.tree, "release").expect("--branch release");

    record_landing(&fixture.tree, &mut in_flight, fixtures::OTHER_SHA)
        .expect("the landing is recorded");

    let written = State::read(&fixture.tree.state_file()).expect("reads");
    assert_eq!(written.branch, "release", "the operator's move survives");
    assert_eq!(
        written.deployed.as_deref(),
        Some(fixtures::OTHER_SHA),
        "and the deploy is recorded by the same write"
    );
    assert_eq!(in_flight.branch, "release", "memory follows the disk");
}

/// fails if a retargeted tree keeps deploying the branch it was set up
/// from. The origin has two branches whose heads differ, so a deploy that
/// read the wrong one lands the wrong sha.
#[tokio::test(start_paused = true)]
async fn a_retargeted_tree_deploys_the_new_branchs_head() {
    let fixture = fixture_with_previous_release();
    fixtures::run_git(fixture.origin.path(), &["checkout", "-q", "-b", "release"]);
    let release_head = commit_on_origin(&fixture, "release-only.txt");
    fixtures::run_git(fixture.origin.path(), &["checkout", "-q", "main"]);
    let main_head = commit_on_origin(&fixture, "main-only.txt");
    assert_ne!(main_head, release_head);

    set_branch(&fixture.tree, "release").expect("--branch release");
    let mut state = State::read(&fixture.tree.state_file()).expect("reads");

    let outcome = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut state,
        &fixtures::dog_config(),
    )
    .await
    .expect("lands");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: release_head.clone()
        }
    );
    assert!(
        fixture
            .tree
            .release(&release_head)
            .join("release-only.txt")
            .exists()
    );
    assert!(
        !fixture
            .tree
            .release(&release_head)
            .join("main-only.txt")
            .exists()
    );
}
