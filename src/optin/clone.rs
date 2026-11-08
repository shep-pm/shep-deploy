//! The bare clone a tree deploys from: making it, linking it, and moving a
//! tree that owns one onto the shared clone.
//!
//! One clone per remote, at `deploy/repos/<key>/git`, and every tree with that
//! remote links its `git` to it (see [`crate::paths::Repo`]). A tree set up
//! before clones were shared owns one inside its own tree instead, and only
//! [`crate::optin::prepare_with`] moves it: a deploy works either way and
//! never changes which clone a tree uses.

use std::path::Path;

use crate::error::Error;
use crate::paths::{Repo, Tree};
use crate::state::is_sha;
use crate::{git, lock, retention, shared};

/// Gives `tree` the bare clone for `remote`: links it to the shared one,
/// making that first if no sheep with this remote has, and moving it off a
/// clone of its own when it owns one.
///
/// Under the clone's lock, so a removal that is sweeping the shared clone for
/// links cannot take it between its creation and this link.
///
/// # Errors
/// [`Error::Config`] for either shape of clone [`relink_owned_clone`] refuses.
/// [`Error::Io`] or [`Error::Git`] as [`git::init_bare`], [`Tree::link_clone`],
/// [`crate::retention::release_clone`], [`drop_unregistered_releases`] and
/// [`crate::lock::hold_clone`] return them.
pub(super) fn share_clone(tree: &Tree, shep_home: &Path, remote: &str) -> Result<(), Error> {
    let repo = Repo::for_remote(shep_home, remote);
    if tree.owns_clone() {
        return relink_owned_clone(tree, shep_home, &repo);
    }
    match tree.linked_clone() {
        Some(old) if old != repo => {
            drop_unregistered_releases(tree)?;
            retention::release_clone(shep_home, tree)?;
        }
        Some(_) => {}
        None => drop_unregistered_releases(tree)?,
    }
    let _cloning = lock::hold_clone(&repo.git())?;
    git::init_bare(&repo.git())?;
    tree.link_clone(&repo)
}

/// Move `tree` off a clone of its own and onto `repo`, removing the clone it
/// owned once no other tree links to it.
///
/// Only `setup` reaches this: [`crate::optin::prepare_with`] refuses a tree
/// with a record, so the clone belongs to a setup that died before shared
/// clones existed.
///
/// The old clone is removed rather than moved into `repos/`: a worktree's
/// `.git` names its clone by absolute path either way, so a move preserves
/// nothing the next fetch does not rebuild.
///
/// # Errors
/// [`Error::Config`] if `tree`'s clone and `repo`'s are one directory, or if
/// another tree links to `tree`'s clone. [`Error::Io`] or [`Error::Git`] as
/// [`crate::lock::hold_clone`], [`crate::retention::tree_linking_to`],
/// [`git::init_bare`], [`drop_unregistered_releases`] and [`Tree::link_clone`] return them.
fn relink_owned_clone(tree: &Tree, shep_home: &Path, repo: &Repo) -> Result<(), Error> {
    let (owned, shared_clone) = (tree.git(), repo.git());
    // `flock` on a second descriptor of one file waits for this process's own
    // hold forever, so the two must be two places before either is locked.
    if shared::same_path(&owned, &shared_clone) {
        return Err(Error::Config(format!(
            "{}'s clone at {} is the same directory as the shared clone for its remote, so \
             relinking the tree would remove the clone it is meant to link to. That is not a \
             layout this dog makes: put {} back as an ordinary directory and run init again",
            tree.sheep(),
            owned.display(),
            tree.root().display()
        )));
    }
    // The clone being removed is held while its users are counted, so a fetch
    // into it by another tree is waited out rather than raced.
    let _removing = lock::hold_clone(&owned)?;
    if let Some(other) = retention::tree_linking_to(shep_home, &owned)? {
        return Err(Error::Config(format!(
            "{} owns its clone at {}, and {} links its `git` there, so removing it would leave \
             that tree with no clone. Point that link at {} and run init again",
            tree.sheep(),
            owned.display(),
            shared::printable(other.display()),
            shared_clone.display()
        )));
    }
    // The clone being linked to is held second, always after the one being
    // removed. Nothing else holds two clone locks at once, so no cycle forms.
    let _linking = lock::hold_clone(&shared_clone)?;
    git::init_bare(&shared_clone)?;
    drop_unregistered_releases(tree)?;
    std::fs::remove_dir_all(&owned).map_err(Error::at(&owned))?;
    tree.link_clone(repo)
}

/// Remove sha-named releases and their completion markers before changing clones
///
/// The new clone cannot prune releases it never registered. Keeping the old
/// clone until cleanup finishes ensures a retry repeats interrupted cleanup.
/// Other entries under `releases/` are the operator's and stay.
///
/// Nothing here serves traffic: setup refuses working directories inside this tree.
///
/// # Errors
/// [`Error::Io`] if `releases/` cannot be listed, or a release or its markers
/// cannot be removed.
fn drop_unregistered_releases(tree: &Tree) -> Result<(), Error> {
    // An interrupted deletion must never leave a marker vouching for a partial release.
    match std::fs::remove_dir_all(tree.completions()) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Io {
                path: tree.completions(),
                source,
            });
        }
    }
    let releases = tree.releases();
    let entries = match std::fs::read_dir(&releases) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::Io {
                path: releases,
                source,
            });
        }
        Ok(entries) => entries,
    };
    for entry in entries {
        let entry = entry.map_err(Error::at(&releases))?;
        let is_release = entry
            .file_name()
            .to_str()
            .is_some_and(|name| is_sha(name) && entry.file_type().is_ok_and(|kind| kind.is_dir()));
        if is_release {
            std::fs::remove_dir_all(entry.path()).map_err(Error::at(&entry.path()))?;
        }
    }
    Ok(())
}
