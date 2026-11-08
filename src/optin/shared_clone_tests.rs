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

/// The directory `sha`'s release lives in for `remote`.
///
/// Every `prepare` in this file runs on a sheep that sets nothing, so the key
/// is the default one.
fn release_of(tree: &Tree, remote: &str, sha: &str) -> std::path::PathBuf {
    tree.release(remote, sha, &fixtures::default_build_key())
}

/// A release the tree built into its own `releases/`, and its marker: what
/// [`clone::drop_unregistered_releases`] sweeps before a tree changes clones.
/// The directories are made, because a marker is written beside its release.
fn superseded(tree: &Tree, sha: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    std::fs::create_dir_all(tree.superseded_completions()).expect("complete/");
    (
        tree.superseded_releases().join(sha),
        tree.superseded_completions().join(sha),
    )
}

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
            std::fs::read_to_string(release_of(tree, &bpm.state.remote, &bpm.sha).join("run.sh"),)
                .expect("release"),
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

#[test]
fn relinking_an_owned_clone_is_refused_while_another_tree_links_to_it_relatively() {
    let home = tempfile::tempdir().expect("tempdir");
    let remote = "https://example.com/owned";
    let web = Tree::for_sheep(home.path(), "web");
    let api = Tree::for_sheep(home.path(), "api");
    git::init_bare(&web.git()).expect("web owns a clone");
    std::fs::create_dir_all(api.root()).expect("api tree");
    std::os::unix::fs::symlink("../web/git", api.git()).expect("relative clone link");

    let err = share_clone(&web, home.path(), remote).expect_err("refuses");

    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(err.to_string().contains("api"), "{err}");
    assert!(web.owns_clone(), "web keeps its clone");
    assert!(api.git().join("HEAD").is_file(), "api still reaches it");
    assert!(!Repo::for_remote(home.path(), remote).git().exists());
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
    let (release, marker) = superseded(&tree, &sha);
    git::worktree_add(&tree.git(), &release, &sha).expect("worktree");
    std::fs::write(&marker, b"").expect("marker");
    std::fs::write(
        tree.superseded_releases().join("notes.txt"),
        "the operator's own",
    )
    .expect("write");

    share_clone(&tree, home.path(), &remote).expect("relinks");

    assert!(!release.exists(), "the unregistered release is gone");
    assert!(!marker.exists(), "and its marker with it");
    assert_eq!(
        std::fs::read_to_string(tree.superseded_releases().join("notes.txt")).expect("reads"),
        "the operator's own",
        "an entry under releases/ that is not a release stays"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_relink_cleanup_invalidates_the_release_before_retrying_setup() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let old_sha = fixtures::head_of(checkout.path());
    std::fs::write(checkout.path().join("second.txt"), "the next release").expect("second file");
    fixtures::run_git(checkout.path(), &["add", "second.txt"]);
    fixtures::run_git(checkout.path(), &["commit", "-q", "-m", "second release"]);
    let remote = checkout.path().to_str().expect("utf-8");
    let tree = Tree::for_sheep(home.path(), "web");
    git::init_bare(&tree.git()).expect("owned clone");
    git::fetch(&tree.git(), remote, "main", fixtures::TEST_BUDGET).expect("fetch");
    let sha = git::remote_head(&tree.git(), "main").expect("head");
    let (old_release, old_marker) = superseded(&tree, &old_sha);
    let (release, marker) = superseded(&tree, &sha);
    git::worktree_add(&tree.git(), &old_release, &old_sha).expect("old worktree");
    git::worktree_add(&tree.git(), &release, &sha).expect("worktree");
    std::fs::write(&old_marker, b"").expect("old marker");
    std::fs::write(&marker, b"").expect("marker");
    let permissions = std::fs::metadata(&old_release)
        .expect("release metadata")
        .permissions();
    std::fs::set_permissions(&old_release, std::fs::Permissions::from_mode(0o555))
        .expect("refuse removal of release contents");

    let result = share_clone(&tree, home.path(), remote);

    std::fs::set_permissions(&old_release, permissions).expect("restore permissions");
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
    assert!(
        !marker.exists(),
        "a failed deletion cannot leave a marker vouching for the release"
    );
    std::fs::remove_file(old_release.join("run.sh")).expect("model a partially deleted release");
    let entries = [("web", checkout.path())];
    let daemon = RollOf::new(&entries);
    let prepared = tokio::time::timeout(
        fixtures::TEST_BUDGET,
        prepare(&daemon, home.path(), "web", &fixtures::dog_config()),
    )
    .await
    .expect("setup finishes within its budget")
    .expect("setup retries");

    assert_eq!(prepared.sha, sha);
    let repo = Repo::for_remote(home.path(), remote);
    let built = repo.release(&sha, &fixtures::default_build_key());
    assert_eq!(
        std::fs::read_to_string(built.join("run.sh")).expect("rebuilt checkout"),
        "#!/bin/sh\necho hi\n",
        "the retry checks the head out again, under the shared releases"
    );
    assert!(
        repo.completion(&sha, &fixtures::default_build_key())
            .is_file(),
        "a finished checkout"
    );
    assert!(
        !old_release.exists(),
        "the retry removes the obsolete release"
    );
    assert!(!old_marker.exists());
}

#[test]
fn changing_remotes_retries_release_cleanup_before_switching_clones() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let origin = fixtures::checkout(&[("run.sh", "#!/bin/sh\necho hi\n")]);
    let old_remote = origin.path().to_str().expect("utf-8");
    let new_remote = "https://example.com/new";
    let tree = Tree::for_sheep(home.path(), "web");
    std::fs::create_dir_all(tree.root()).expect("tree");
    share_clone(&tree, home.path(), old_remote).expect("shares old clone");
    let old = Repo::for_remote(home.path(), old_remote);
    git::fetch(&tree.git(), old_remote, "main", fixtures::TEST_BUDGET).expect("fetch");
    let sha = git::remote_head(&tree.git(), "main").expect("head");
    let (release, marker) = superseded(&tree, &sha);
    git::worktree_add(&tree.git(), &release, &sha).expect("worktree");
    std::fs::write(&marker, b"").expect("marker");
    let permissions = std::fs::metadata(&release).expect("metadata").permissions();
    std::fs::set_permissions(&release, std::fs::Permissions::from_mode(0o555))
        .expect("refuse cleanup");

    let result = share_clone(&tree, home.path(), new_remote);

    std::fs::set_permissions(&release, permissions).expect("restore permissions");
    assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
    assert_eq!(tree.linked_clone(), Some(old.clone()));
    assert!(old.git().join("HEAD").is_file(), "the old clone is kept");
    assert!(!marker.exists());

    share_clone(&tree, home.path(), new_remote).expect("retries cleanup and relinks");

    assert!(!release.exists(), "the retry removes the obsolete release");
    assert!(!old.root().exists(), "the unused old clone is released");
    assert_eq!(
        tree.linked_clone(),
        Some(Repo::for_remote(home.path(), new_remote))
    );
}
