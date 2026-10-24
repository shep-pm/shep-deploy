//! Tests for setup sharing one bare clone between sheep with one remote.

use std::path::Path;

use super::prepare::prepare;
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
