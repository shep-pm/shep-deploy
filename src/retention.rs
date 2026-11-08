//! Reclaiming release worktrees once they are past the retention count.
//!
//! # What keeps this from deleting a directory something is running from
//!
//! A process's working directory is resolved when it is spawned, so an
//! instance started before a swap goes on executing from the old release
//! even after `current` moves. Removing that release out from under a
//! running process is the hazard this module has to avoid. It runs after
//! EVERY ending of a deploy, verified or not, so the argument has to hold
//! for each of them, and it rests on three things rather than on timing:
//!
//! - The release `current` names is spared whatever its age.
//! - The release `deploy.toml` names is spared too. The two agree except
//!   for a deploy killed between its swap and its record write, which is
//!   exactly the case where the record's release is what the next rollback
//!   returns to, and that release is not always the newest by mtime.
//! - A release that just failed is the newest by mtime, and `keep` is at
//!   least two (`crate::config` refuses less), so it is never a candidate in
//!   the cycle that follows its own failure. After a verified deploy every
//!   instance has turned over, so nothing runs from anything older.
//!
//! "Retention could delete what something is running from" is the right
//! worry to have about this module, so here is the rest of the answer: [`prune`]
//! reads `releases/` and nothing else. A sheep's pre-adoption instance runs
//! from the operator's own checkout, which is not in there. [`release_clone`]
//! reads the other trees' records for the same reason - what is running from a
//! shared build cache is not visible from the cache itself.
//!
//! # Why a failure here never fails a deploy
//!
//! It runs after the deploy's own outcome is decided and recorded, at which
//! point `current`, the record and the running process agree with each
//! other and the deploy is genuinely over. A worktree that cannot be removed
//! costs disk, not correctness, and reporting the deploy as failed because
//! of it would report a failure that did not happen.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Error;
use crate::paths::{self, Repo, Tree};
use crate::state::{State, is_sha};
use crate::{git, shared, swap};

/// Removes every release beyond the newest `keep`, and answers with the
/// shas it removed.
///
/// Never removes the release `current` names, nor any that `recorded` names
/// (the record's `deployed`, in memory and on disk), whatever their age. The
/// first is what is serving; the others are what a rollback returns to when
/// a deploy died between its swap and its record write, or wrote its record
/// and could not. See the module doc for why both are needed.
///
/// A release's completion marker (see [`Tree::completion`]) goes with it,
/// and so does any marker whose release is already gone by other means:
/// nothing else ever removes one, so without this the tree gained a file
/// per commit ever deployed, forever.
///
/// # Errors
/// [`Error::Io`], naming `releases/`, if it cannot be listed.
/// [`Error::Git`] if a worktree cannot be removed or, when every removal
/// went, if the bookkeeping cannot be pruned. A caller is expected to warn
/// on these rather than fail the deploy that triggered them: see this
/// module's own doc.
pub fn prune(tree: &Tree, keep: usize, recorded: &[&str]) -> Result<Vec<String>, Error> {
    let live = swap::resolve(&tree.current())?;
    let live = live.as_deref().and_then(sha_of);

    let mut found = Vec::new();
    let releases = tree.releases();
    let entries = match fs::read_dir(&releases) {
        Ok(entries) => entries,
        // A target with no releases directory has nothing to reclaim,
        // which is every target before its first deploy.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: releases,
                source,
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(Error::at(&releases))?;
        let (Some(name), Ok(modified)) = (
            entry.file_name().to_str().map(str::to_owned),
            entry.metadata().and_then(|meta| meta.modified()),
        ) else {
            continue;
        };
        // Only real release directories reach the ordering below. Every
        // release is a worktree keyed by a full git object id, so anything
        // else under `releases/` is not this module's to reason about, and
        // treating it as a release is not harmless: an entry with a fresh
        // mtime takes one of the `keep` slots and pushes a real release into
        // the doomed set. The one it pushes out is the second newest, which
        // is the release a failed deploy rolls back to.
        if !names_a_release(&entry, &name) {
            continue;
        }
        found.push((name, modified));
    }

    let git_dir = tree.git();
    let mut removed = Vec::new();
    let mut failure = None;
    let spared: Vec<Option<&str>> = live
        .as_deref()
        .into_iter()
        .chain(recorded.iter().copied())
        .map(Some)
        .collect();
    for sha in doomed(&found, keep, &spared) {
        match git::worktree_remove(&git_dir, &tree.release(&sha)) {
            Ok(()) => {
                // The marker vouched for a checkout that is gone now.
                let _ = fs::remove_file(tree.completion(&sha));
                removed.push(sha);
            }
            // One release that will not go must not strand the rest, and
            // must not skip the prune below. A `?` here did both, and it
            // did them every cycle: whatever made the removal fail was
            // still there next time, so nothing ordered after it was ever
            // reclaimed and the registration cleanup never ran at all.
            Err(err) => failure = failure.or(Some(err)),
        }
    }

    // Unconditionally, not only when this cycle removed something. `prune`
    // exists for worktrees that vanished by some other means, an operator's
    // own `rm -rf` or a crash mid-build, and gating it on our own removals
    // stopped it running in exactly that case. A hand-removed release then
    // stayed registered, and redeploying that sha failed with "is a missing
    // but already registered worktree".
    //
    // Its failure is reported only when no removal failed: a `?` here threw
    // away the removal failure the loop above went out of its way to keep.
    // A failure of either kind still costs the caller the list of what was
    // removed, which the one caller does not read.
    let pruned = git::worktree_prune(&git_dir);
    sweep_markers(tree, &releases);

    match failure.or(pruned.err()) {
        Some(err) => Err(err),
        None => Ok(removed),
    }
}

/// Lets go of the shared clone `tree` links to, removing the clone when no
/// other tree links to it. Answers whether it removed the clone.
///
/// For the tree's own removal, never for retention: [`prune`] reclaims
/// worktrees and nothing else. A tree is counted as a link whether or not it
/// has a record yet, so a setup still fetching into the clone keeps it. See
/// [`tree_linking_to`] for how the other trees are found. Under the clone's
/// lock, so a setup linking to it at the same moment either lands before the
/// sweep and keeps it, or finds it gone and makes it again.
///
/// A tree with a clone of its own, or none, is left as it is.
///
/// The build cache beside the clone goes only with the last tree that deploys
/// its remote, and that is not only the trees linking to the clone. A tree that
/// owns its clone builds against that cache and links every release's `target`
/// into it, so removing it would leave one serving from a dangling link, and
/// would take the cache out from under a build running at that moment. No link
/// says such a tree is there; its record is the only thing that does, so the
/// cache stays while another tree owns a clone and its record names the remote.
/// See [`another_tree_deploys`]. No build turn is needed for the same reason: a
/// tree with a record is the only one that can be building here, and a setup
/// without one links its `git` to the clone under this lock before it builds.
///
/// # Errors
/// [`Error::Io`] if the lock cannot be taken, the tree's link cannot be
/// removed, the deploy directory cannot be listed, or the clone cannot be
/// removed.
pub fn release_clone(shep_home: &Path, tree: &Tree) -> Result<bool, Error> {
    let Some(repo) = tree.linked_clone() else {
        return Ok(false);
    };
    let _cloning = crate::lock::hold_clone(&repo.git())?;
    match fs::remove_file(tree.git()) {
        Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::Io {
                path: tree.git(),
                source,
            });
        }
        _ => {}
    }

    if tree_linking_to(shep_home, &repo.git())?.is_some() {
        return Ok(false);
    }
    let doomed = if another_tree_deploys(shep_home, tree, &repo)? {
        repo.git()
    } else {
        repo.root().to_owned()
    };
    fs::remove_dir_all(&doomed).map_err(Error::at(&doomed))?;
    Ok(true)
}

/// Whether a tree other than `tree` owns a clone of its own and has a record
/// naming the remote `repo` keys.
///
/// The shape matters. A tree linked to `repo`'s clone is counted by
/// [`tree_linking_to`], which runs first. A tree with neither a link nor a clone
/// has nothing to build with, and that is exactly a restored sheep: its `git` is
/// unlinked here and its record stays. A tree that owns a clone is the one this
/// cannot see any other way, and its record is the only thing that says which
/// remote it deploys.
///
/// A record that cannot be read counts as one that does: a cache left behind
/// costs disk, and one removed from under a live release costs the release.
///
/// `tree` is skipped, being the tree whose clone is going. Every directory under
/// `deploy` but `repos` is asked, whether or not its name can be a sheep's.
///
/// # Errors
/// [`Error::Io`], naming `deploy/`, if it cannot be listed.
fn another_tree_deploys(shep_home: &Path, tree: &Tree, repo: &Repo) -> Result<bool, Error> {
    let deploy = shep_home.join("deploy");
    for entry in fs::read_dir(&deploy).map_err(Error::at(&deploy))? {
        let entry = entry.map_err(Error::at(&deploy))?;
        let (name, root) = (entry.file_name(), entry.path());
        if name == paths::REPOS || root == tree.root() {
            continue;
        }
        if !fs::symlink_metadata(root.join("git")).is_ok_and(|meta| meta.file_type().is_dir()) {
            continue;
        }
        let record = root.join("deploy.toml");
        if !record.is_file() {
            continue;
        }
        match State::remote_at(&record) {
            Some(remote) if tree.repo(&remote) == *repo => return Ok(true),
            Some(_) => {}
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// The first tree directory under `shep_home/deploy` whose `git` links to
/// `git_dir`, or `None` when no tree does.
///
/// The trees are the directories under `deploy/`, not [`paths::targets`], which
/// lists only the ones with a record: a tree whose setup has not written one
/// yet is fetching into the clone right now. `repos` is skipped, being where
/// the clones themselves live. A link counts by the place it resolves to, so
/// one clone spelled two ways is one clone.
///
/// A caller about to remove `git_dir` holds [`crate::lock::hold_clone`] for it
/// while it asks, and unlinks its own tree first.
///
/// # Errors
/// [`Error::Io`], naming `deploy/`, if it cannot be listed.
pub fn tree_linking_to(shep_home: &Path, git_dir: &Path) -> Result<Option<PathBuf>, Error> {
    let deploy = shep_home.join("deploy");
    for entry in fs::read_dir(&deploy).map_err(Error::at(&deploy))? {
        let entry = entry.map_err(Error::at(&deploy))?;
        if entry.file_name() == paths::REPOS {
            continue;
        }
        let Ok(target) = fs::read_link(entry.path().join("git")) else {
            continue;
        };
        if shared::same_path(&entry.path().join(target), git_dir) {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

/// Removes every completion marker whose release is no longer on disk.
///
/// Best effort throughout: a marker is a zero-byte file whose only reader
/// is `deploy::prepare::checkout_release`, and one that cannot be removed costs a
/// stale entry, which that reader already guards against by also requiring
/// the directory.
fn sweep_markers(tree: &Tree, releases: &Path) {
    let Ok(markers) = fs::read_dir(tree.completions()) else {
        return;
    };
    for marker in markers.flatten() {
        if !releases.join(marker.file_name()).exists() {
            let _ = fs::remove_file(marker.path());
        }
    }
}

/// Whether an entry under `releases/` is a release rather than something else.
///
/// A release is a directory named by a full git object id, because that is
/// the only thing [`crate::paths::Tree::release`] ever creates there. A
/// stray file, an editor's backup, a directory an operator made by hand: none
/// of those are releases, and none may occupy one of the `keep` slots.
///
/// `file_type` does not follow symlinks, deliberately. A symlinked entry is
/// not a worktree either.
fn names_a_release(entry: &fs::DirEntry, name: &str) -> bool {
    entry.file_type().is_ok_and(|kind| kind.is_dir()) && is_sha(name)
}

/// Which of `releases` to remove: everything past the newest `keep`, minus
/// any of `spared`.
///
/// Ordered by directory modification time rather than by commit date. A
/// release's directory is created when its worktree is added and written
/// into by its build, so its mtime is when this host last worked on it,
/// which is the question retention is asking. Commit date is a fact about
/// the repository and would put a redeployed older sha in the wrong place.
fn doomed(releases: &[(String, SystemTime)], keep: usize, spared: &[Option<&str>]) -> Vec<String> {
    let mut ordered: Vec<&(String, SystemTime)> = releases.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ordered
        .into_iter()
        .skip(keep)
        .filter(|(name, _)| !spared.contains(&Some(name.as_str())))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The sha a release path names, which is its last component.
fn sha_of(release: &Path) -> Option<String> {
    release
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;
