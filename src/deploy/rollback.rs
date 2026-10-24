//! Putting `current` back, and the running sheep with it, after a release
//! that did not come up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::deploy::record::write_record;
use crate::deploy::reload::{reload_until, sha_of};
use crate::error::Error;
use crate::paths::Tree;
use crate::state::State;
use crate::swap;
use crate::verify;
use crate::window::{budget, dwell_for};

/// Puts `current` back after a deploy that never started anything.
///
/// No reload, deliberately, and no [`Error::Split`]. Nothing was spawned on
/// the release this swap installed, so the running process is the one that
/// was already there and the symlink is the only thing out of step; once it
/// is back, `current`, `deploy.toml` and the process all agree again and
/// there is nothing left for an operator to do. Sending a reload here would
/// restart a healthy sheep for no reason, and reporting a split state would
/// describe a machine that is not split - the variant reserved for what this
/// crate cannot repair, spent on something it just did.
///
/// Which release to put back is [`rollback_target`]'s decision, the same
/// one a full rollback makes: `previous` first, then the record. `previous`
/// is `None` only when `current` was absent at the start of the deploy,
/// which no path in this crate produces once a cutover has landed, so it
/// means an operator removed the link by hand. The record still names the
/// release that was serving, and that is what goes back. Leaving `current`
/// on the unverified release instead, as this once did, was a swap nothing
/// had approved reported as an ordinary failure.
///
/// # Residual, and it is the same one the `deploy` module doc names for
/// `current.tmp`
///
/// Between the swap and the reload there is a window a handful of syscalls
/// and one RPC wide. A reload started by somebody else inside it would spawn
/// from the new `current`, and undoing the swap would then leave that
/// process on a release `current` no longer names. Nothing here detects
/// that. The check it would take - re-reading the flock and comparing pids -
/// cannot tell that case apart from an ordinary crash restart, so it would
/// report a split state for a healthy machine, which is the defect this
/// function exists to stop rather than a cure for it.
///
/// # Errors
/// [`Error::RollbackFailed`] carrying `why` if `current` cannot be put back,
/// or if there is nothing on disk to put it back to. Both leave the same
/// thing for an operator: `current` naming a release nothing is running.
pub(super) fn undo_swap(
    tree: &Tree,
    state: &State,
    previous: Option<&Path>,
    attempted: &str,
    why: &Error,
) -> Result<(), Error> {
    let wrap = |source| Error::RollbackFailed {
        why: why.to_string(),
        source: Box::new(source),
    };

    let Some(target) = rollback_target(tree, state, previous, attempted) else {
        return Err(wrap(Error::Config(format!(
            "current names {attempted}, which nothing verified, and neither the link it \
             replaced nor deploy.toml names an earlier release that is still on disk. Point {} \
             at a release by hand before deploying again",
            tree.current().display()
        ))));
    };

    swap::point_at(&tree.current(), &target).map_err(wrap)
}

/// Puts `previous` back after `attempted` was rejected, and answers with
/// the sha the target is live on again.
///
/// A failure while rolling back is wrapped in [`Error::RollbackFailed`]
/// alongside `why`, at this one place, so a caller never has to choose
/// between the failure that made a rollback necessary and the failure of
/// the rollback itself. Both are what an operator needs.
///
/// [`Error::Split`] and [`Error::RollbackUnverified`] are the exceptions and
/// pass through untouched. Each already carries `why`, and wrapping `Split`
/// produced a message that ended by telling an operator to go and compare the
/// three things its inner half had just finished telling them. The second is
/// not a rollback that failed: the swap, the record and the reload all
/// worked, and what is missing is proof the release came up.
///
/// "Nothing to roll back to" is not wrapped that way, because no rollback
/// was attempted: there was nothing to attempt one against.
/// [`Error::Unverified`] says the whole thing in one sentence instead. It
/// is the failure a new user is most likely to meet, a first deploy that
/// never comes up, and framing it as a rollback that failed described a
/// machine nobody could recognise.
///
/// Whether there is anything to roll back to at all is
/// [`rollback_target`]'s decision, and it reads both `current` and
/// `deploy.toml` rather than only the first.
///
/// It compares shas, not paths. Both spellings name the same release when
/// they agree, but `previous` is link text read off disk and `attempted` is
/// a sha this process just resolved, and the two need not be written the
/// same way: `$SHEP_HOME` reaches this crate through
/// [`std::path::absolute`], which does not resolve symlinks, and the dog
/// and the operator's own CLI can each arrive at a different literal path
/// for one directory. A sha comparison has no such degree of freedom.
///
/// # Errors
/// [`Error::Unverified`] if [`rollback_target`] finds nothing to roll back
/// to: neither `current` nor `deploy.toml` names an earlier release that is
/// still on disk.
/// [`Error::Split`] or [`Error::RollbackUnverified`], unwrapped, if that is
/// what [`restore`] returned.
/// [`Error::RollbackFailed`] wrapping anything else it returned.
#[expect(clippy::too_many_arguments, reason = "one rollback, one call site")]
pub(super) async fn roll_back<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    app: &AppConfig,
    previous: Option<&Path>,
    attempted: &str,
    why: &str,
    patience: Duration,
) -> Result<String, Error> {
    let Some(previous) = rollback_target(tree, state, previous, attempted) else {
        return Err(Error::Unverified {
            sheep: tree.sheep().to_owned(),
            sha: attempted.to_owned(),
            why: why.to_owned(),
        });
    };

    let to = sha_of(&previous);
    restore(
        daemon, tree, state, app, &previous, attempted, why, patience,
    )
    .await
    // `Split` already carries `why`, and says more about the machine
    // than the wrapper's own tail could - wrapping it produced a
    // message that ended by telling an operator to go and check the
    // three things the inner half had just told them.
    .map_err(|source| match source {
        passed @ (Error::Split { .. } | Error::RollbackUnverified { .. }) => passed,
        source => Error::RollbackFailed {
            why: why.to_owned(),
            source: Box::new(source),
        },
    })?;

    Ok(to)
}

/// Which release a rollback should return to, or `None` when there is
/// genuinely no earlier one.
///
/// Two places know of an earlier release and they can disagree, so both are
/// asked. `current` is the first choice: it is what was serving a moment
/// ago. `deploy.toml` is the second, and it is not a nicety - a deploy
/// killed between its swap and its `State::write` is precisely the
/// interruption [`restore`]'s record correction exists to repair, and it
/// leaves `current` already naming the release being attempted while the
/// record still names a good one. Reading `current` alone, the retry
/// concluded that `previous == attempted` meant a first deploy with nothing
/// to roll back to, and left a release that would not come up serving.
///
/// A candidate has to name a release other than the one that just failed,
/// and it has to still be on disk. Both conditions apply to both sources,
/// which they did not at first: retention removes old worktrees, so neither
/// a sha in `deploy.toml` nor the target of `current` is a promise that
/// anything is there to point at, and a rollback onto a path that does not
/// exist would swap `current` to a dangling link - a sheep whose `cwd`
/// resolves to nothing, which is worse than the release that would not come
/// up. Guarding only the record's branch left the first source able to do
/// exactly the harm the guard was added for.
fn rollback_target(
    tree: &Tree,
    state: &State,
    previous: Option<&Path>,
    attempted: &str,
) -> Option<PathBuf> {
    // A candidate has to be a release: a full sha, other than the one that
    // just failed, with a directory on disk. `previous` is the raw text of a
    // link an operator can repoint by hand, and a `current -> /srv/anything`
    // used to make `restore` write `deployed = "anything"`, which
    // `State::read` then refused on every later run of every command.
    let usable = |release: PathBuf| {
        let sha = sha_of(&release);
        (crate::state::is_sha(&sha) && sha != attempted && release.exists()).then_some(release)
    };

    if let Some(release) = previous.map(Path::to_path_buf).and_then(usable) {
        return Some(release);
    }

    let recorded = state.deployed.as_deref()?;
    usable(tree.release(recorded))
}

/// Points `current` back at `previous`, corrects the record to match, and
/// reloads onto it.
///
/// The three steps are in that order and none of them is optional.
///
/// The record is written BEFORE the reload rather than after it, which is
/// the one ordering choice here worth arguing. Written after, a reload the
/// shepherd would not accept leaves `current` naming the old release and
/// `deploy.toml` naming whatever it named before - two disagreements to
/// explain instead of one. Written first, the filesystem and the record
/// always agree with each other, and the only thing that can be out of step
/// is the running process, which is exactly what [`Error::Split`] says. It
/// also leaves the record naming a release the next poll will see as behind
/// the branch head, so an unattended target retries rather than settling.
///
/// The reload is then checked like a deploy's own, by `state.verify` against
/// the flock as it was just before the reload. The budget and the dwell come
/// from `app`, the sheep's stored config, which no release changes;
/// `patience` bounds the retries of the reload itself. The reload being accepted says the shepherd began a
/// swap and nothing about the release coming up: one whose binary cannot be
/// spawned is dropped from the flock after the acceptance. The generation
/// comes from [`reload_until`], taken before the attempt that was accepted.
/// A capture that fails is no reason to skip the reload; it leaves nothing to
/// verify against, and is reported as unverified once the reload is sent.
///
/// The reload is not best-effort: a swap back that is never reloaded leaves
/// the old code on disk and the new, unhealthy instance still running, and
/// an operator told the rollback succeeded. Moving a symlink does not move
/// a running process.
///
/// The record is corrected rather than assumed already right. `deploy` only
/// advances `state.deployed` after a verify, so it usually already names
/// `previous` - but a deploy killed between its `Reload` and its
/// `State::write` leaves `deploy.toml` lagging `current`, and without this
/// write the next rollback would repair the filesystem and leave the record
/// wrong indefinitely. Written only when it actually differs, so the
/// ordinary rollback costs no write at all.
///
/// # Errors
/// [`Error::Io`] if `current` cannot be repointed or `deploy.toml` cannot be
/// written. [`Error::Split`] if the reload could not be issued within
/// `patience` and the failure was one that might yet have cleared - see
/// [`reload_until`] for which those are, and [`Error::Split`] for what an
/// operator is told. The reload's own error, unchanged and unwrapped, if it
/// was one that never could have cleared: a `NotFound` is that failure and
/// not a split, since there is no sheep for a process to be running.
/// [`Error::RollbackUnverified`] if the reload was accepted and the release
/// was not seen to come up, or the generation before it could not be taken.
#[expect(clippy::too_many_arguments, reason = "one rollback, one call site")]
async fn restore<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    app: &AppConfig,
    previous: &Path,
    attempted: &str,
    why: &str,
    patience: Duration,
) -> Result<(), Error> {
    let sheep = tree.sheep();
    let to = sha_of(previous);

    swap::point_at(&tree.current(), previous)?;

    if state.deployed.as_deref() != Some(to.as_str()) {
        state.deployed = Some(to.clone());
        write_record(tree, state)?;
    }

    let before = match reload_until(daemon, sheep, patience).await {
        Ok(before) => before,
        // A failure that could have cleared and did not is the split state:
        // the reload may yet be pending, so the process may be on either
        // release. A failure that could never clear is not - it is just
        // that failure, and dressing it as a split would claim a running
        // process that a `NotFound` says is not there.
        Err(source) if source.is_retryable() => {
            return Err(Error::Split {
                sheep: sheep.to_owned(),
                on: to,
                running: attempted.to_owned(),
                why: why.to_owned(),
                source: Box::new(source),
            });
        }
        Err(source) => return Err(source),
    };
    let reloaded_at = Instant::now();

    let unverified = |found: String| Error::RollbackUnverified {
        sheep: sheep.to_owned(),
        to: to.clone(),
        why: why.to_owned(),
        found,
    };
    let before = before.map_err(|source| unverified(source.to_string()))?;
    match verify::wait(
        daemon,
        sheep,
        state.verify,
        &before,
        budget(app, before.instances()),
        dwell_for(app),
    )
    .await
    {
        Ok(true) => Ok(()),
        Ok(false) => Err(unverified(format!(
            "it did not come up and stay up, {}s after the rollback's reload",
            reloaded_at.elapsed().as_secs()
        ))),
        Err(source) => Err(unverified(source.to_string())),
    }
}

#[cfg(test)]
mod tests;
