//! Reclaiming release worktrees once they are past the retention count.
//!
//! # What keeps this from deleting a directory something is running from
//!
//! A process's working directory is resolved when it is spawned, so an
//! instance started before a swap goes on executing from the old release
//! even after `current` moves. Removing that release out from under a running
//! process is the hazard this module has to avoid. It runs after
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
//! - The build a repository shares - the clone under `repos/<key>` and the
//!   cache beside it - is spared while any sheep of that repository names it,
//!   from its `current` or from its record. See [`release_clone`].
//!
//! "Retention could delete what something is running from" is the right
//! worry to have about this module, so here is the rest of the answer. A
//! release is a repository's, not a tree's: every sheep that deploys one
//! remote builds into the same `releases/` directory, and one sheep's prune is
//! therefore a sweep over directories other sheep may be serving from. [`spared`]
//! is what makes that safe. It reads every tree beside the one being deployed
//! before anything is removed - what each one's `current` links to, and what
//! each one's record names - and a release named by any of them is not a
//! candidate. What else is shared is the build, and neither the clone nor the
//! cache can say who uses it, so [`release_clone`] asks the same two questions
//! of every tree before it lets go of one.
//!
//! Sparing is deliberately wider than the exact release a record means. A
//! record names a sha, and the directory that sha's release lives in also
//! carries the build key, which cannot be recomputed from a record alone. So a
//! record spares every build variant of the sha it names. A stale variant costs
//! disk; a live one removed costs a running service its working directory.
//!
//! # Why a failure here never fails a deploy
//!
//! It runs after the deploy's own outcome is decided and recorded, at which
//! point `current`, the record and the running process agree with each
//! other and the deploy is genuinely over. A worktree that cannot be removed
//! costs disk, not correctness, and reporting the deploy as failed because
//! of it would report a failure that did not happen.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Error;
use crate::paths::{self, Repo, Tree};
use crate::state::State;
use crate::{git, lock, shared, swap};

/// Removes every release of the repository `remote` names beyond the newest
/// `keep`, and answers with the release names it removed.
///
/// Releases live beside the clone, shared by every sheep that deploys that
/// remote, so this is one sweep of one directory rather than one per tree. It
/// never removes a release any tree's `current` names, nor one whose sha any
/// tree's record names (the record's `deployed` and `staged`, in memory and on
/// disk), whatever their age. See [`spared`] for what is asked of which tree.
///
/// A release's completion marker (see [`Tree::completion`]) goes with it, and
/// so does any marker whose release is already gone by other means: nothing
/// else ever removes one, so without this the repository gained a file per
/// commit ever deployed, forever.
///
/// # Errors
/// [`Error::Io`], naming `deploy/` or `releases/`, if either cannot be listed.
/// [`Error::Git`] if a worktree cannot be removed or, when every removal went,
/// if the bookkeeping cannot be pruned. A caller is expected to warn on these
/// rather than fail the deploy that triggered them: see this module's own doc.
pub fn prune(
    tree: &Tree,
    remote: &str,
    keep: usize,
    recorded: &[&str],
) -> Result<Vec<String>, Error> {
    let repo = tree.repo(remote);
    // The repository build turn, held for the whole sweep. Without it this reads
    // `deploy/` and removes directories with nothing synchronising the two, and a
    // deploy that has let go of the turn but not yet swapped is holding a release
    // no `current` names and no record names yet - exactly the one a `keep` count
    // has no witness for. Holding the turn means a prune either runs before that
    // deploy takes it, in which case the release it removes carries no completion
    // marker either and the deploy checks out and builds afresh, or after its
    // claim (see [`crate::swap::claim`]) is written, in which case the claim is a
    // witness and the release is not a candidate.
    //
    // So a prune queues behind the repository's builds. It costs disk, not
    // correctness, when one is skipped or delayed, and this module's own rule is
    // that a prune never fails the deploy that triggered it.
    let _building = lock::hold_build(&repo)?;
    let (releases, completions) = (repo.releases(), repo.completions());
    let spared = spared(tree.deploy_dir(), &repo, recorded)?;

    let mut found = Vec::new();
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
        // release is a worktree keyed by a full git object id and a build key,
        // so anything else under `releases/` is not this module's to reason
        // about, and treating it as a release is not harmless: an entry with a
        // fresh mtime takes one of the `keep` slots and pushes a real release
        // into the doomed set. The one it pushes out is the second newest,
        // which is the release a failed deploy rolls back to.
        if !paths::is_release_name(&name) || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        found.push((name, modified));
    }

    let git_dir = tree.git();
    let mut removed = Vec::new();
    let mut failure = None;
    for name in doomed(&found, keep, &spared) {
        let release = releases.join(&name);
        if !registered_in(&release, &git_dir) {
            // A release another clone registered, which is a tree that still
            // owns its clone building into this shared directory. `git worktree
            // remove` in THIS clone refuses it every cycle, so without this the
            // prune reported a git failure forever and never reclaimed
            // anything ordered after it. That tree's own prune reclaims it with
            // the clone that knows it.
            continue;
        }
        match git::worktree_remove(&git_dir, &release) {
            Ok(()) => {
                // The marker vouched for a release that is gone now.
                let _ = fs::remove_file(completions.join(&name));
                removed.push(name);
            }
            // One release that will not go must not strand the rest, and
            // must not skip the prune below. A `?` here did both, and it
            // did them every cycle: whatever made the removal fail was
            // still there next time, so nothing ordered after it was ever
            // reclaimed and the registration cleanup never ran at all.
            //
            // A release another clone registered is one of these. A tree that
            // owns its clone builds into this directory too, and `git worktree
            // remove` only knows its own registrations; that tree's own prune
            // reclaims it, with its own clone.
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
    sweep_markers(&completions, &releases);

    match failure.or(pruned.err()) {
        Some(err) => Err(err),
        None => Ok(removed),
    }
}

/// What one repository's retention must not remove.
///
/// Two sets, because the two witnesses name a release differently: a `current`
/// link names the directory itself, and a record names only the sha, whose
/// directory also carries a build key no record can be read back into. See the
/// module doc for why the wider answer is the right one.
#[derive(Debug, Default)]
struct Spared {
    /// Release directory names, from what a tree's `current` resolves to.
    names: BTreeSet<String>,
    /// Shas, from what a tree's record names as deployed or staged.
    shas: BTreeSet<String>,
}

impl Spared {
    fn holds(&self, name: &str) -> bool {
        self.names.contains(name) || self.shas.contains(paths::release_sha(name))
    }
}

/// Asks every tree beside `tree` what it is still using, for one repository.
///
/// `recorded` is the calling deploy's own `deployed` and `staged`, in memory as
/// well as on disk: the two can differ after a verified deploy whose record
/// write failed, and both are releases something might still be serving.
///
/// Every directory under `deploy` but `repos` is asked, whether or not its name
/// can be a sheep's, and whether or not its `git` links this clone: a tree that
/// owns a clone of its own builds into this repository's `releases/` too. A
/// record that cannot be read names nothing, which is safe here: a tree whose
/// record cannot be read deploys nothing and rolls back to nothing, and its
/// `current` is spared by the first half anyway.
///
/// # Errors
/// [`Error::Io`], naming `deploy/`, if it cannot be listed.
fn spared(deploy: &Path, repo: &Repo, recorded: &[&str]) -> Result<Spared, Error> {
    let mut spared = Spared {
        names: BTreeSet::new(),
        shas: recorded.iter().map(|sha| (*sha).to_owned()).collect(),
    };
    let releases = repo.releases();
    for entry in fs::read_dir(deploy).map_err(Error::at(deploy))? {
        let entry = entry.map_err(Error::at(deploy))?;
        if entry.file_name() == paths::REPOS {
            continue;
        }
        let root = entry.path();
        if let Some(name) = serving(&root.join("current"), &releases) {
            spared.names.insert(name);
        }
        if let Some(name) = claiming(&root.join("current"), &releases) {
            spared.names.insert(name);
        }
        let record = root.join("deploy.toml");
        if !record.is_file() {
            continue;
        }
        let Some((remote, shas)) = State::releases_at(&record) else {
            continue;
        };
        if paths::repo_at(deploy, &remote) == *repo {
            spared.shas.extend(shas);
        }
    }
    Ok(spared)
}

/// The release directory name a tree's `current` names, when it names one under
/// `releases`.
///
/// The link is read and its parent compared by where it resolves to, so one
/// directory spelled two ways is one directory (see [`shared::same_path`]). A
/// `current` pointing anywhere else - an operator's own checkout, say - is not
/// this dog's release and spares nothing.
fn serving(current: &Path, releases: &Path) -> Option<String> {
    named_in(&shared::resolved(&swap::resolve(current).ok()??), releases)
}

/// The release directory name a tree claims it is about to serve, for the window
/// between a deploy letting go of the repository build turn and repointing
/// `current`. See [`crate::swap::claim`].
///
/// A claim that cannot be read is no claim, which is the same rule [`spared`]
/// applies to a record it cannot read: the tree is asked nothing, and the tree
/// whose own deploy is holding that release spares it by its `current` a moment
/// later anyway.
fn claiming(current: &Path, releases: &Path) -> Option<String> {
    named_in(&shared::resolved(&swap::claimed(current)?), releases)
}

/// The file name of `named`, when its parent is `releases`.
fn named_in(named: &Path, releases: &Path) -> Option<String> {
    let (name, parent) = (named.file_name()?, named.parent()?);
    shared::same_path(parent, releases).then(|| name.to_string_lossy().into_owned())
}

/// Whether the worktree at `release` is one `git_dir` has registered.
///
/// A linked worktree carries a `.git` FILE naming the clone that made it, as
/// `gitdir: <clone>/worktrees/<name>`, and not every release in this directory
/// was made by this tree's clone: a tree that still owns a clone of its own
/// builds into the same shared `releases/`. `git worktree remove` in a clone
/// that never registered one fails, so this decides whether this prune may
/// touch the release at all.
///
/// A release whose `.git` cannot be read counts as this clone's. Whatever is
/// wrong with it, `git worktree remove` and `git worktree prune` are the tools
/// that can still fix it, and refusing to try would strand it.
fn registered_in(release: &Path, git_dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(release.join(".git")) else {
        return true;
    };
    let Some(gitdir) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(|gitdir| shared::resolved(&release.join(gitdir.trim())))
    else {
        return true;
    };
    gitdir.starts_with(shared::resolved(git_dir))
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
/// The rest of the repository directory - the cache every sheep of this remote
/// compiles into, and the releases they build into it - goes only with the last
/// sheep that names it, and naming it is a per-repository question. The trees
/// linking to the clone are the easy half: while any of them is there the whole
/// repository directory stays. A tree that owns a clone of its own is the other
/// half, and [`another_tree_uses`] reads both of its witnesses, its record and
/// its `current`. No build turn is needed here: a tree with a record is the only
/// one that can be building, and a setup without one links its `git` to the
/// clone under this lock before it builds.
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
    let doomed = if another_tree_uses(shep_home, tree, &repo)? {
        repo.git()
    } else {
        repo.root().to_owned()
    };
    fs::remove_dir_all(&doomed).map_err(Error::at(&doomed))?;
    Ok(true)
}

/// Whether a tree other than `tree` still uses the repository `repo` holds: its
/// clone, the cache beside it, and the releases built into it.
///
/// A tree linked to `repo`'s clone is counted by [`tree_linking_to`], which runs
/// first. A tree with neither a link nor a clone has nothing to build with - a
/// restored sheep, which keeps nothing. A tree that owns a clone is asked both
/// questions that can be asked of it:
///
/// - its record names the remote `repo` keys, so its next build compiles into
///   this cache and into these releases;
/// - its `current` names a release in `repo`'s `releases/`, or resolves through
///   a release linked at `repo`'s cache. A record cannot say this one:
///   [`crate::optin::prepare_with`] swaps `current` before it writes the record,
///   so a run killed between the two leaves a built release with no record at
///   all.
///
/// A record that cannot be read counts as one that does: a cache left behind
/// costs disk, and one removed from under a live release costs the release.
///
/// `tree` is skipped, being the tree whose clone is going. Every directory under
/// `deploy` but `repos` is asked, whether or not its name can be a sheep's.
///
/// # Errors
/// [`Error::Io`], naming `deploy/`, if it cannot be listed.
fn another_tree_uses(shep_home: &Path, tree: &Tree, repo: &Repo) -> Result<bool, Error> {
    let (deploy, cache) = (shep_home.join("deploy"), repo.cache_target());
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
        if record.is_file() {
            match State::releases_at(&record) {
                Some((remote, _)) if tree.repo(&remote) == *repo => return Ok(true),
                Some(_) => {}
                None => return Ok(true),
            }
        }
        let current = root.join("current");
        if serving(&current, &repo.releases()).is_some() || serves_from(&current, &cache) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the tree whose `current` is `current` has a live release whose
/// `target` link lands in the build cache `cache`.
///
/// The link is read rather than followed, and compared by where it resolves to,
/// so one cache spelled two ways is one cache (see [`shared::same_path`]). A
/// `target` the release ships itself - a directory, or a link naming somewhere
/// this crate never linked - is not this dog's build and does not count.
fn serves_from(current: &Path, cache: &Path) -> bool {
    let Ok(Some(release)) = swap::resolve(current) else {
        return false;
    };
    let Ok(named) = fs::read_link(release.join("target")) else {
        return false;
    };
    shared::same_path(&release.join(named), cache)
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
/// is `deploy::prepare::checkout_release`, and one that cannot be removed costs
/// a stale entry, which that reader already guards against by also requiring
/// the directory.
fn sweep_markers(completions: &Path, releases: &Path) {
    let Ok(markers) = fs::read_dir(completions) else {
        return;
    };
    for marker in markers.flatten() {
        if !releases.join(marker.file_name()).exists() {
            let _ = fs::remove_file(marker.path());
        }
    }
}

/// Which of `releases` to remove: everything past the newest `keep`, minus any
/// `spared` holds.
///
/// Ordered by directory modification time rather than by commit date. A
/// release's directory is created when its worktree is added and written
/// into by its build, so its mtime is when this host last worked on it,
/// which is the question retention is asking. Commit date is a fact about
/// the repository and would put a redeployed older sha in the wrong place.
fn doomed(releases: &[(String, SystemTime)], keep: usize, spared: &Spared) -> Vec<String> {
    let mut ordered: Vec<&(String, SystemTime)> = releases.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ordered
        .into_iter()
        .skip(keep)
        .filter(|(name, _)| !spared.holds(name))
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests;
