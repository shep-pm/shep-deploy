//! A deploy whether the tree's `git` is a clone of its own or a link to a
//! shared one.

use std::fs;

use crate::deploy::test_support::{Shepherd, commit_on_origin, fixture_with_previous_release};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::fixtures;
use crate::paths::Repo;

/// fails if a tree set up before clones were shared stops deploying. Its
/// `git` is a directory, no link, and migrating it is a later change: until
/// then a deploy must not need anything the link brings.
#[tokio::test]
async fn a_tree_with_its_own_clone_still_deploys() {
    let mut fixture = fixture_with_previous_release();
    assert!(fixture.tree.owns_clone(), "the fixture's clone is its own");
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
    assert!(fixture.tree.release(&second).join("second.txt").is_file());
}

/// fails if a deploy through a `git` link cannot fetch, add a worktree for the
/// new release or prune: every one of them runs with the link as its
/// directory, and the worktree's own `.git` pointer names the clone behind it.
#[tokio::test]
async fn a_tree_linked_to_a_shared_clone_deploys() {
    let mut fixture = fixture_with_previous_release();
    let repo = Repo::for_remote(fixture.home.path(), &fixture.state.remote);
    fs::create_dir_all(repo.root()).expect("repo dir");
    fs::rename(fixture.tree.git(), repo.git()).expect("move the clone");
    fixture.tree.link_clone(&repo).expect("link");
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
    assert!(fixture.tree.release(&second).join("second.txt").is_file());
    assert!(
        repo.git().join("worktrees").is_dir(),
        "the worktree is registered in the shared clone"
    );
}

/// fails if a deploy fetches its recorded remote into a shared clone keyed by
/// another one. The record is hand-editable, and the fetch's mirror refspec
/// with `--prune` would replace every branch the other sheep deploy from.
#[tokio::test]
async fn a_tree_linked_for_one_remote_refuses_to_deploy_another() {
    let mut fixture = fixture_with_previous_release();
    let repo = Repo::for_remote(fixture.home.path(), &fixture.state.remote);
    fs::create_dir_all(repo.root()).expect("repo dir");
    fs::rename(fixture.tree.git(), repo.git()).expect("move the clone");
    fixture.tree.link_clone(&repo).expect("link");
    let before = crate::git::remote_head(&repo.git(), "main").expect("head");
    fixture.state.remote = "https://example.invalid/other.git".to_owned();

    let err = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("refused");

    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(err.to_string().contains("setup"), "{err}");
    assert_eq!(
        crate::git::remote_head(&repo.git(), "main").expect("head"),
        before,
        "the clone is untouched"
    );
}
