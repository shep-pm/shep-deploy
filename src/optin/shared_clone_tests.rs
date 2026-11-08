//! Tests for setup sharing one bare clone between sheep with one remote, and
//! for moving a tree that owns one onto it.

use std::path::Path;

use super::clone::share_clone;
use super::prepare::prepare;
use super::test_support::{RollOf, checkout_with_commit};
use crate::error::Error;
use crate::fixtures;
use crate::git;
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

/// fails if `setup` leaves a tree that owns its clone owning it. The clone is
/// the repository's whole object store, and a tree that never migrates keeps a
/// private copy of it for as long as it is deployed.
#[test]
fn a_tree_that_owns_its_clone_is_relinked_and_its_old_clone_removed() {
    let home = tempfile::tempdir().expect("tempdir");
    let remote = "https://example.com/owned";
    let tree = Tree::for_sheep(home.path(), "web");
    git::init_bare(&tree.git()).expect("a clone of its own");
    std::fs::write(tree.git().join("left-behind"), "objects").expect("its own objects");
    assert!(tree.owns_clone());

    share_clone(&tree, home.path(), remote).expect("relinks");

    let repo = Repo::for_remote(home.path(), remote);
    assert_eq!(tree.linked_clone(), Some(repo.clone()));
    assert_eq!(
        std::fs::read_link(tree.git()).expect("a link, not a directory"),
        repo.git()
    );
    assert!(repo.git().join("HEAD").is_file(), "the shared clone exists");
    assert!(
        !repo.git().join("left-behind").exists(),
        "the old clone is removed, not moved into repos/"
    );
}

/// fails if relinking a tree takes the clone another tree is using. Nothing in
/// this dog links a tree to another tree's clone, so this is a hand-made link,
/// and removing what a link points at is the one thing a relink must not do.
#[test]
fn relinking_an_owned_clone_is_refused_while_another_tree_links_to_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let remote = "https://example.com/owned";
    let trees = ["web", "api"].map(|sheep| {
        let tree = Tree::for_sheep(home.path(), sheep);
        std::fs::create_dir_all(tree.root()).expect("tree");
        tree
    });
    git::init_bare(&trees[0].git()).expect("web owns a clone");
    std::os::unix::fs::symlink(trees[0].git(), trees[1].git()).expect("api links to it");

    let err = share_clone(&trees[0], home.path(), remote).expect_err("refuses");

    let shown = err.to_string();
    assert!(matches!(err, Error::Config(_)), "{shown}");
    assert!(shown.contains("api"), "it names the tree at risk: {shown}");
    assert!(
        trees[0].git().join("HEAD").is_file(),
        "the clone web owns is intact"
    );
    assert!(
        trees[1].git().join("HEAD").is_file(),
        "api still reaches it"
    );
    assert!(
        !Repo::for_remote(home.path(), remote).git().exists(),
        "a refused relink makes no clone either"
    );
}

/// fails if `setup` on a tree an older version left with its own clone keeps
/// that clone. Re-running `init` is the only way such a tree is migrated, and
/// the tree it produces must be the one every later deploy fetches through.
#[tokio::test]
async fn setup_relinks_a_tree_that_owns_its_clone() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let tree = Tree::for_sheep(home.path(), "bpm");
    git::init_bare(&tree.git()).expect("a clone of its own, as an older setup left it");
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    let repo = Repo::for_remote(home.path(), &prepared.state.remote);
    assert_eq!(prepared.tree.linked_clone(), Some(repo.clone()));
    assert!(!prepared.tree.owns_clone(), "the tree's own clone is gone");
    assert_eq!(
        git::remote_head(&repo.git(), "main").expect("the shared clone has the fetched head"),
        prepared.sha
    );
}

/// fails if a relink leaves the release the old clone had checked out. The new
/// clone never registered it, so `git worktree remove` refuses it: it would
/// survive every later prune, and every prune would report a failure for it.
#[test]
fn a_relink_drops_the_release_the_old_clone_had_checked_out() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = fixtures::checkout(&[("run.sh", "#!/bin/sh\necho hi\n")]);
    let remote = origin.path().to_str().expect("utf-8").to_owned();
    let tree = Tree::for_sheep(home.path(), "web");
    git::init_bare(&tree.git()).expect("a clone of its own");
    git::fetch(&tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");
    let sha = git::remote_head(&tree.git(), "main").expect("head");
    git::worktree_add(&tree.git(), &tree.release(&sha), &sha).expect("worktree");
    std::fs::create_dir_all(tree.completions()).expect("complete/");
    std::fs::write(tree.completion(&sha), b"").expect("marker");
    std::fs::write(tree.releases().join("notes.txt"), "the operator's own").expect("write");

    share_clone(&tree, home.path(), &remote).expect("relinks");

    assert!(
        !tree.release(&sha).exists(),
        "the unregistered release is gone"
    );
    assert!(!tree.completion(&sha).exists(), "and its marker with it");
    assert_eq!(
        std::fs::read_to_string(tree.releases().join("notes.txt")).expect("reads"),
        "the operator's own",
        "an entry under releases/ that is not a release stays"
    );
}
