//! Tests for setup sharing one bare clone between sheep with one remote.

use std::path::Path;

use super::prepare::{prepare, share_clone};
use super::test_support::{RollOf, checkout_with_commit};
use crate::fixtures;
use crate::paths::{self, Repo, Tree};

/// A second checkout of `first`, so both name `first` as their `origin`.
fn checkout_of(first: &Path) -> tempfile::TempDir {
    let second = tempfile::tempdir().expect("tempdir");
    fixtures::run_git(
        second.path(),
        &[
            "clone",
            "-q",
            first.to_str().expect("utf-8"),
            second.path().to_str().expect("utf-8"),
        ],
    );
    second
}

/// fails if two sheep set up from one remote each get a clone of their own.
/// The clone is the repository's whole object store, so a second copy costs
/// its size again for every sheep that shares a remote.
#[tokio::test]
async fn two_sheep_with_one_remote_leave_one_bare_clone_and_two_links() {
    let home = tempfile::tempdir().expect("tempdir");
    let first = checkout_with_commit();
    let second = checkout_of(first.path());
    let entries = [("bpm", first.path()), ("ctm", second.path())];
    let daemon = RollOf::new(&entries);

    let bpm = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("bpm prepares");
    let ctm = prepare(&daemon, home.path(), "ctm", &fixtures::dog_config())
        .await
        .expect("ctm prepares");

    let clones: Vec<_> = std::fs::read_dir(home.path().join("deploy").join(paths::REPOS))
        .expect("the shared clones")
        .flatten()
        .collect();
    assert_eq!(clones.len(), 1, "one clone: {clones:?}");
    let clone = Repo::for_remote(home.path(), &bpm.state.remote).git();
    assert!(clone.join("HEAD").is_file(), "a bare repository");
    for tree in [&bpm.tree, &ctm.tree] {
        let link = std::fs::read_link(tree.git()).expect("a link, not a directory");
        assert_eq!(link, clone, "{} links to the shared clone", tree.sheep());
        assert_eq!(
            std::fs::read_to_string(tree.release(&bpm.sha).join("run.sh")).expect("release"),
            "#!/bin/sh\necho hi\n"
        );
    }
}

/// fails if `repos` can be a sheep: its tree would be the directory every
/// shared clone lives in, and the next `setup` would write clones inside it.
#[tokio::test]
async fn a_sheep_cannot_be_called_repos() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("repos", checkout.path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "repos", &fixtures::dog_config())
        .await
        .expect_err("refuses");

    assert!(err.to_string().contains("shared"), "{err}");
    assert!(!Tree::for_sheep(home.path(), "repos").root().exists());
}

/// fails if a setup retried after the sheep's remote changed leaves the old
/// shared clone behind with nothing linking to it, or takes it from a sheep
/// that still does.
#[test]
fn relinking_to_another_remote_releases_the_old_clone_when_nothing_else_uses_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let (old, new) = ("https://example.com/old", "https://example.com/new");
    let trees = ["web", "api"].map(|sheep| {
        let tree = Tree::for_sheep(home.path(), sheep);
        std::fs::create_dir_all(tree.root()).expect("tree");
        share_clone(&tree, home.path(), old).expect("shares");
        tree
    });
    let (old, new) = (
        Repo::for_remote(home.path(), old),
        Repo::for_remote(home.path(), new),
    );

    share_clone(&trees[0], home.path(), "https://example.com/new").expect("relinks");
    assert!(
        old.git().join("HEAD").is_file(),
        "api still uses the old one"
    );
    assert_eq!(trees[0].linked_clone(), Some(new.clone()));

    share_clone(&trees[1], home.path(), "https://example.com/new").expect("relinks");
    assert!(!old.root().exists(), "nothing links to the old clone now");
    assert_eq!(trees[1].linked_clone(), Some(new));
}
