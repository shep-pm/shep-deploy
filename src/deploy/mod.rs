//! One deploy, start to finish, and the one setting an operator can change
//! without doing one.
//!
//! [`deploy`] runs the sequence the design spec lays out: fetch, compare,
//! worktree add, link the shared files, build, swap `current`, reload,
//! verify, and on failure swap back and reload again.
//!
//! # Nothing before the swap touches the running app
//!
//! That is the property the whole design is built on, and it is worth
//! saying where it comes from rather than trusting the step order alone: a
//! release is built in `releases/<sha>`, a directory the live release does
//! not share, and the live release is reached through `current`, which
//! nothing moves until the build has already exited zero. A fetch that
//! fails, a worktree that cannot be created, a build that exits three -
//! each of those returns from this module with `current` still pointing
//! exactly where it did, and the app still serving from it. This is what
//! replaces the hardcoded sleep between a build and a restart in the deploy
//! scripts this dog exists to retire.
//!
//! # `Reload`, not `Restart`
//!
//! shep's reload runs `SpawnNew`, `AwaitReady`, `DrainOld`, `ReapOld`. The
//! replacement reaches readiness before the old instance drains, so the old
//! release serves throughout and the new one's startup cost - a vite
//! compile, for the app this was designed against - is paid while the old
//! one is still answering.
//!
//! # Two deploys of one sheep
//!
//! Two things, in this order. [`go`] takes an exclusive `flock` on the
//! tree before anything else and holds it to the end, so two PROCESSES
//! cannot both be inside a deploy of one sheep: the second is refused in one
//! sentence rather than colliding somewhere inside git. See [`crate::lock`]
//! for what that collision did before the lock existed. [`set_watch`] does
//! NOT take it, on purpose; it and every whole-record write a deploy makes
//! take the record lock instead, and its own doc says how a `--watch` typed
//! mid-tick and the tick's record write stay out of each other's way.
//!
//! Behind the lock there is still one cheap guard: `current` is read when a
//! deploy starts and read again immediately before the swap, and a deploy
//! whose `current` moved in between refuses rather than swapping on top of
//! whatever moved it. The lock makes that unreachable through this module,
//! and it stays for anything that moves `current` without taking the lock,
//! an operator's own hand, say. The failure it exists for is specific and
//! silent: a *newer* release, verified and healthy, reverted by an older
//! deploy's swap with nothing anywhere reporting a problem.
//!
//! # A stale `current.tmp`
//!
//! [`crate::swap::point_at`] replaces a temporary symlink left by an
//! interrupted swap, because the lock above means one on disk cannot belong
//! to a deploy that is running now. Anything at that name that is NOT a
//! symlink was not left by this dog, and is refused with the path to move
//! aside.

mod landing;
mod prepare;
mod record;
mod reload;
mod rollback;
#[cfg(test)]
pub(crate) mod test_support;

use std::path::PathBuf;

use shep_client::shep_core::config::AppConfig;

use crate::build;
use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::Tree;
use crate::settings::Settings;
use crate::state::State;
use crate::{git, linkcheck, lock, settings, shared, skip, swap};

use record::record_landing;

pub(crate) use landing::{run_release_hook, swap_and_land};
pub(crate) use prepare::{
    Staged, head_of, read_staged, refuse_ungated_at_setup, refuse_ungated_verification,
    stage_release,
};
pub(crate) use record::record;
pub use record::set_watch;

/// What one deploy did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The tracked branch's head is already deployed.
    UpToDate,
    /// A new release was built, swapped in, and verified.
    Deployed {
        /// The sha now live.
        sha: String,
    },
    /// A new release was swapped in, did not come up, and the previous
    /// release was put back.
    RolledBack {
        /// The sha that is live again.
        to: String,
        /// Why the new release was rejected.
        why: String,
    },
    /// The branch moved, but nothing under the target's the sheep's `reload`
    /// paths changed, so nothing was built or reloaded and the record is as it
    /// was. Only [`unattended`] answers this.
    Skipped {
        /// The head that was left alone.
        sha: String,
    },
}

/// Deploys the head of `state.branch` to the tree's sheep, rolling back if
/// it does not come up.
///
/// Returns [`Outcome::UpToDate`] without touching anything when the remote
/// head is already deployed, which is the ordinary answer for a poll that
/// found nothing new. `state.deployed` advances only after a verify
/// succeeds, and `deploy.toml` is written only when it does, so an
/// interrupted or rolled-back deploy leaves the record naming whatever is
/// actually serving.
///
/// A release directory is reused rather than checked out again only when it
/// carries the completion marker, which is written after `git worktree add`
/// returns: it is the leftover of a finished earlier attempt at this same
/// sha. A directory without the marker is the remains of a checkout that died
/// partway, so it is removed and checked out afresh - see
/// [`prepare::stage_release`]. The build runs over the release either way.
///
/// # Errors
/// [`Error::AlreadyDeploying`] if another process holds the tree, and
/// [`Error::Io`] if the lock file cannot be created - both from
/// [`lock::hold`], which is the first thing that runs. Then
/// [`Error::NotCutOver`] if the target's record names no released sha - see
/// the refusal in the body for why that deploy would report a success it
/// did not achieve, and the variant's own doc for why the poll loop needs to
/// recognise it.
///
/// Anything the steps before the swap return - see [`crate::git::fetch`],
/// [`crate::shared::to_link`] and
/// [`crate::build::run`] - in which case nothing has been disturbed and the
/// old release is still serving. [`Error::Raced`] if `current` moved while
/// this deploy was preparing, which is refused rather than swapped over.
///
/// After the swap, an error from the reload or from verification is rolled
/// back exactly as a failed verification is - swap, reload, correct the
/// record - and then returned as [`Error::RolledBack`], which names what is
/// live again and keeps the original reachable through
/// [`core::error::Error::source`]. The deploy still failed, so it is still
/// an error, but an operator reading it can tell a rollback happened.
///
/// A failure BEFORE the shepherd accepted the reload - the generation
/// capture, or the reload itself being refused - means nothing was ever
/// started on the new release, so `current` is put back and that failure is
/// returned as it arrived, unwrapped. The machine is left exactly as it was
/// before the deploy, which is why there is nothing extra to say about it.
///
/// A rollback that itself fails gives one of four errors, and a caller
/// that wants to detect "the rollback did not work" has to accept all of
/// them rather than matching [`Error::RollbackFailed`] alone:
///
/// - [`Error::Split`], bare, when the reload could not be issued and the
///   failure was one that might yet clear. It carries both halves itself,
///   which is why it is not wrapped.
/// - [`Error::RollbackFailed`], wrapping anything else the rollback met -
///   a swap that could not be made, a record that could not be written, a
///   reload refused for a reason that can never clear.
/// - [`Error::RollbackUnverified`], bare, when the reload was accepted and
///   the release put back was not seen to come up, under the same
///   `state.verify` a deploy is judged by. It exits `1`, not `12`.
/// - [`Error::Unverified`], bare, when there was no earlier release to
///   return to at all. No rollback was attempted, so nothing frames it as
///   one that failed.
///
/// [`Error::RollbackFailed`] is also what a failed `undo_swap` gives, for
/// the same reason it wraps a failed `restore`: `current` is left naming a
/// release nothing is running.
///
/// One failure past the swap is reported and NOT returned: `deploy.toml`
/// refusing to be written after a successful verify. The new release is
/// live and verified, there is nothing to undo and undoing would take a
/// working release out of service, so the outcome is still
/// [`Outcome::Deployed`] and the failure is printed. What lags is the
/// record, and only on disk: `state.deployed` has advanced in this process,
/// so the poll loop's next tick reads the target as up to date, while a
/// restarted dog reads the old sha off disk and deploys the same one again.
/// One rebuild, and it repairs itself.
///
/// `keep` is the retention count: [`crate::retention`] reclaims every
/// release beyond the newest `keep`, sparing whatever `current` names
/// regardless of age. It runs after every ending, not only a verified one -
/// the target that keeps failing is the one building a fresh worktree for
/// every commit somebody pushes, and pruning only on success left exactly
/// that one growing with the branch. A prune failure is reported with
/// `eprintln!` rather than returned - see that module's own doc - so it
/// never turns a deploy that already succeeded into one this function
/// reports as failed.
///
/// Every ending that is not [`Outcome::Deployed`] is written into
/// `state.failed`, which is what [`unattended`] holds on and what an
/// operator asking by name ignores. See [`record::record`].
pub async fn deploy<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    go(daemon, tree, state, config, Held::Retry).await
}

/// [`deploy`], as the poll loop asks for it: a sha an earlier attempt
/// failed on is left alone until the branch moves.
///
/// The difference is only the trigger, exactly as `manual` is. Everything
/// else - releases, shared linking, the atomic swap, probe verification,
/// auto-rollback and retention - is the same code.
///
/// A branch that moved without touching anything the live release's
/// the sheep's `reload` paths name is [`Outcome::Skipped`]: nothing is
/// checked out, built or reloaded, and the record is not written, so the next
/// comparison is still against the last sha actually deployed. See
/// [`crate::skip`]. A failure to decide deploys as if there were no table.
///
/// # Errors
/// [`Error::Held`] if the branch head is the sha `state.failed` names, in
/// which case nothing at all was attempted. Otherwise as [`deploy`].
pub async fn unattended<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    go(daemon, tree, state, config, Held::Hold).await
}

/// What an entry point does about a sha the last attempt failed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Try it again. An operator asking for a deploy by name is asking for
    /// exactly the attempt the loop is declining to make on its own.
    Retry,
    /// Leave it alone until the branch moves.
    Hold,
}

/// The body both entries share. See [`deploy`].
async fn go<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    config: &DogConfig,
    held: Held,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();

    // Held for the whole of this function, and dropped with it. Everything
    // below writes to the tree, and the poll loop's no-overlap guarantee
    // covers one tick against another IN THIS PROCESS and nothing else: the
    // README tells an operator to run `shep-deploy deploy <sheep>` by hand
    // while the dog is polling. See `crate::lock` for what that collision
    // actually did.
    let _deploying = lock::hold(tree)?;

    // The one state where a deploy is not merely wrong but silently
    // convincing, and the reason it is refused in code rather than in prose.
    //
    // `deployed` is None on an existing tree only when the cutover never
    // landed: `prepare` writes the record with None and `cut_over` is what
    // fills it in, so nothing else produces this. The sheep is therefore
    // still registered at the operator's OWN checkout. A deploy from here
    // builds, swaps `current`, and reloads the sheep BY NAME - which really
    // does replace that instance, so `crate::verify::wait` sees a full pid
    // turnover, the deploy reports success for a release nothing ever
    // served, and the sha is written into the record.
    //
    // Three messages already tell an operator not to do this, and a fourth
    // would not help. The poll loop no longer arrives here on its own -
    // `prepare` writes `watch = "manual"` and the cutover is what promotes
    // it, so an abandoned tree is passed over rather than refused once an
    // interval forever - but this stays, because `shep deploy <sheep>` is
    // one command away and a record can be edited by hand.
    if state.deployed.is_none() {
        return Err(Error::NotCutOver {
            sheep: sheep.to_owned(),
            tree: tree.root().to_owned(),
        });
    }

    let started_at = swap::resolve(&tree.current())?;

    // Off the runtime's thread: see `shared::off_thread` for what a fetch on
    // it used to stall. `run_git_within` bounds the wait; this keeps the
    // dog answering during it.
    let (git_dir, remote, tracked, budget) = (
        tree.git(),
        state.remote.clone(),
        state.branch.clone(),
        config.git_timeout,
    );
    shared::off_thread(move || git::fetch(&git_dir, &remote, &tracked, budget)).await?;
    let head = head_of(tree, &state.branch)?;
    if state.deployed.as_deref() == Some(head.as_str()) {
        return Ok(Outcome::UpToDate);
    }

    // The whole of what `Held::Hold` changes, and it is deliberately here
    // rather than before the fetch: the branch moving is what clears a
    // hold, and the fetch is how this finds out that it did.
    if held == Held::Hold && state.failed.as_deref() == Some(head.as_str()) {
        return Err(Error::Held {
            sheep: sheep.to_owned(),
            sha: head,
        });
    }

    // Once per deploy, and after the two returns above so a poll that finds
    // nothing new asks the shepherd for nothing. Returned rather than
    // recorded: the branch will not move to repair it, so a hold on this sha
    // would outlive the operator's fix.
    let (app, settings) = settings::held(daemon, sheep).await?;

    // After the hold, so a sha that already failed is still said once rather
    // than judged again, and only for the poll loop: a hand deploy means now.
    if held == Held::Hold && skip::nothing_changed(tree, state, &settings.reload, &head).await {
        return Ok(Outcome::Skipped { sha: head });
    }

    let outcome = attempt(
        daemon, tree, state, &head, started_at, &app, &settings, config,
    )
    .await;
    record(daemon, tree, state, &head, &outcome, config.retention).await;
    outcome
}

/// Everything from the release directory to the verdict, for one sha that
/// is going to be attempted: the checkout and the build, then
/// [`swap_and_land`].
///
/// Split out of [`go`] so that recording what the attempt came to has one
/// place to happen rather than five. See [`deploy`] for what each ending
/// means.
#[expect(clippy::too_many_arguments, reason = "one deploy, one call site")]
async fn attempt<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    head: &str,
    started_at: Option<PathBuf>,
    app: &AppConfig,
    settings: &Settings,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();
    let release = tree.release(head);
    let staged = {
        let (tree, checkout, head) = (tree.clone(), state.checkout.clone(), head.to_owned());
        let (app, settings) = (app.clone(), settings.clone());
        shared::off_thread(move || stage_release(&tree, &checkout, &head, &app, &settings)).await?
    };
    refuse_ungated_verification(sheep, &staged.app, state.verify)?;
    let snapshot = linkcheck::Snapshot::before(sheep, &release, &staged.linked);
    let built = build::run(
        sheep,
        &release,
        &staged.spec,
        staged.app.user.as_deref(),
        &config.passthrough,
        &tree.cache_target(),
        config.build_timeout,
    )
    .await;
    // Printed whether or not the build succeeded: a write into the checkout
    // happened either way.
    for warning in snapshot.after() {
        eprintln!("{warning}");
    }
    built?;
    run_release_hook(tree, head, &staged, config).await?;

    swap_and_land(daemon, tree, state, head, started_at, &staged, config).await
}

#[cfg(test)]
mod tests {
    mod clones;
    mod hooks;
    mod retarget;
    mod settings;
    mod skip;

    use std::fs;

    use crate::deploy::test_support::{
        Shepherd, commit_on_origin, fixture_before_any_release, fixture_with_previous_release,
        test_config_keeping,
    };
    use crate::error::Error;
    use crate::state::State;
    use crate::{fixtures, swap};

    use super::*;

    /// fails if a deploy that never comes up leaves the new release live.
    /// This is the whole promise of the feature: verification without
    /// rollback is just a slower way to be broken.
    #[tokio::test(start_paused = true)]
    async fn a_release_that_never_comes_up_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }));
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a deploy stops running its steps in the order build, release
    /// hook, swap and reload, record, after hook. The two halves of `attempt`
    /// are joined by a call, and this pins that the join drops no step and
    /// reorders none: each step appends its name to one file in the release,
    /// and the after hook also copies the record it found.
    ///
    /// Not paused, for the reason on `a_current_that_moved_during_the_build_is_not_swapped_over`.
    #[tokio::test]
    async fn attempt_behaves_as_before_after_the_split() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready().with_settings(
            "command = 'echo build >> order.txt'\n\
             [release]\ncommand = 'echo release >> order.txt'\n\
             [after]\ncommand = 'echo after >> order.txt'\n",
        );

        let outcome = deploy(
            &daemon,
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
        let release = fixture.tree.release(&second);
        assert_eq!(
            fs::read_to_string(release.join("order.txt")).expect("the steps ran"),
            "build\nrelease\nafter\n"
        );
        assert_eq!(daemon.reload_count(), 1);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(release)
        );
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.deployed.as_deref(), Some(second.as_str()));
        assert_eq!(written.failed, None);
    }

    /// fails if an unchanged remote head still does the work. Polling must
    /// be cheap; a deploy per tick would rebuild constantly.
    #[tokio::test]
    async fn an_unchanged_head_does_nothing() {
        let mut fixture = fixture_with_previous_release();
        let outcome = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");
        assert!(matches!(outcome, Outcome::UpToDate));
    }

    /// fails if a release that comes up healthy is not recorded as
    /// deployed. `state.deployed` is what the next poll compares against,
    /// so a deploy that succeeds without advancing it would redeploy the
    /// same sha on every tick forever.
    #[tokio::test(start_paused = true)]
    async fn a_release_that_comes_up_is_deployed_and_recorded() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();

        let outcome = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        // Swapping the symlink is not deploying. Without this the whole
        // reload can be deleted and the test still passes.
        assert_eq!(daemon.reload_count(), 1, "the sheep must be reloaded once");
        assert_eq!(
            outcome,
            Outcome::Deployed {
                sha: second.clone()
            }
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&second))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(second.as_str()));

        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml was written");
        assert_eq!(written.deployed.as_deref(), Some(second.as_str()));
    }

    /// fails if two processes can deploy one sheep at once.
    ///
    /// The poll loop's no-overlap guarantee is one `for` loop in one process.
    /// README.md tells an operator to run `shep-deploy deploy <sheep>` by hand
    /// to retry a held commit, and the dog is polling the whole time, so two
    /// processes on one tree is a documented workflow rather than a corner
    /// case. Round 10 reproduced both of them losing to git's own index lock
    /// and the loser then recording a failure for the sha the winner deployed.
    ///
    /// The hold here stands in for that other process.
    #[tokio::test]
    async fn a_deploy_is_refused_while_another_process_holds_the_tree() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();
        let held = crate::lock::hold(&fixture.tree).expect("the other process");

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("must refuse while the tree is held");

        assert!(
            matches!(&err, Error::AlreadyDeploying { sheep } if sheep == "web"),
            "must name the sheep rather than fail somewhere inside git: {err:?}"
        );
        assert_eq!(daemon.reload_count(), 0, "nothing may have been attempted");
        drop(held);
    }

    /// fails if the loop attempts a sha it has already failed on. The whole
    /// point of writing it down: two reloads of a live app and a full
    /// rebuild every thirty seconds, indefinitely, is what an unheld one
    /// costs. The branch moving is what clears it, which is what CI does
    /// with a red commit.
    #[tokio::test(start_paused = true)]
    async fn a_failed_sha_is_held_until_the_branch_moves() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        let daemon = Shepherd::never_ready();
        unattended(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("rolls back");
        let reloads = daemon.reload_count();

        let err = unattended(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("holds");

        assert!(matches!(err, Error::Held { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains(&second), "{shown}");
        assert_eq!(daemon.reload_count(), reloads, "nothing was reloaded");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous)),
            "and nothing was swapped"
        );
    }

    /// fails if an operator asking for a deploy by name is answered with
    /// the loop's hold. They are asking for exactly the attempt the loop is
    /// declining to make on its own - after fixing the machine the build
    /// needed, most often - and answering "I am not trying that one" would
    /// leave them with no way to ask.
    #[tokio::test(start_paused = true)]
    async fn an_operator_asking_by_name_retries_a_held_sha() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");

        unattended(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("rolls back");

        let daemon = Shepherd::ready();
        let outcome = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("tries again");

        assert!(matches!(outcome, Outcome::Deployed { .. }), "{outcome:?}");
        assert_eq!(daemon.reload_count(), 1);
    }

    /// fails if the hold outlives the commit it was about. A push is the
    /// signal that somebody has done something about the failure, and a
    /// hold that survived it would need a command nobody knows to run.
    #[tokio::test(start_paused = true)]
    async fn a_new_commit_clears_the_hold() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        unattended(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("rolls back");

        let third = commit_on_origin(&fixture, "third.txt");
        let outcome = unattended(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("deploys the new commit");

        assert_eq!(outcome, Outcome::Deployed { sha: third });
        assert_eq!(fixture.state.failed, None, "the hold is cleared");
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("reads")
                .failed,
            None
        );
    }

    /// fails if a failing build reaches the swap. Steps one through five
    /// must never touch the running app: the build happens in a directory
    /// the live release does not share, so a build that exits non-zero has
    /// to leave `current` and `state.deployed` exactly where they were.
    #[tokio::test]
    async fn a_failing_build_never_moves_current() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let err = deploy(
            &Shepherd::ready().with_settings("command = 'exit 3'\n"),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("the build fails");

        assert!(matches!(err, Error::Build { status: Some(3) }), "{err:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a tree the cutover never landed on can be deployed. This is
    /// the deploy that does not merely go wrong, it goes wrong
    /// convincingly. `deployed` is `None` on an existing tree only when
    /// `cut_over` never ran, so the sheep is still registered at the
    /// operator's OWN checkout: a deploy builds, swaps `current`, and
    /// reloads the sheep BY NAME, which really does replace that instance,
    /// so verification sees a full pid turnover, the run prints
    /// `deployed <sha>` and exits 0, and the sha is written into the
    /// record. Measured against a real shepherd on a release whose script
    /// is `exit 1`.
    ///
    /// Three messages already tell an operator not to do this and none of
    /// them refused it. Prose is also the wrong instrument here: an
    /// abandoned tree is now left `manual`, so the poll loop passes over
    /// it, but `shep deploy <sheep>` still reaches this by hand.
    #[tokio::test(start_paused = true)]
    async fn a_tree_the_cutover_never_landed_on_is_refused_before_anything_happens() {
        let mut fixture = fixture_before_any_release();

        let err = deploy(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("refuses");

        // Its own variant rather than `Config`, because this refusal
        // cannot clear on its own and the poll loop has to be able to tell
        // it from a config problem somebody is about to fix. Matched here
        // so nothing quietly folds it back into `Config`, where the only
        // way to recognise it again would be to match on the message text.
        assert!(matches!(err, Error::NotCutOver { .. }), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("shep-deploy setup web"), "{shown}");
        assert_eq!(fixture.state.deployed, None, "nothing was recorded");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            None,
            "and nothing was swapped: the refusal is before any of it"
        );
    }

    /// fails if a deploy swaps over a `current` that moved while it was
    /// building. The release that moved it may be newer and already
    /// verified, so swapping on top of it silently reverts something that
    /// is live and healthy - and nothing anywhere reports a problem. The
    /// build command below is what a concurrent deploy's swap looks like
    /// from inside this one.
    // NOT paused, unlike almost every other test here, and it cannot be.
    // This is the one test that runs a real build command, and `build::run`
    // bounds that command with `tokio::time::timeout`. Under `start_paused`
    // tokio advances the clock to the next deadline whenever the runtime has
    // nothing to do, and a real subprocess makes it idle, so the build times
    // out immediately however large the budget is.
    //
    // The cost is what the previous comment here was protecting against: if
    // the raced-swap guard ever stops holding, this falls through to a real
    // verification window and fails in about ninety seconds, which reads as a
    // hang rather than as the regression it is. Slow and correct beats fast
    // and impossible.
    #[tokio::test]
    async fn a_current_that_moved_during_the_build_is_not_swapped_over() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon =
            Shepherd::never_ready().with_settings("command = 'ln -sfn \"$PWD\" ../../current'\n");

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("current moved under it");

        assert!(matches!(err, Error::Raced { .. }), "{err:?}");
        assert_eq!(daemon.reload_count(), 0, "no reload may be sent at all");
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if retention reclaims the release `--stage` built. It is not
    /// `current` and not `deployed`, and it is older than the newest `keep`
    /// as soon as a few commits land after it, so without its own mention in
    /// the spared list the next deploy of the target throws away the build
    /// the operator staged to avoid waiting for.
    #[tokio::test(start_paused = true)]
    async fn retention_spares_the_staged_release() {
        let mut fixture = fixture_with_previous_release();
        let staged = commit_on_origin(&fixture, "second.txt");
        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch");
        crate::git::worktree_add(&fixture.tree.git(), &fixture.tree.release(&staged), &staged)
            .expect("the staged release");
        fixture.state.staged = Some(staged.clone());

        for name in ["third.txt", "fourth.txt", "fifth.txt"] {
            commit_on_origin(&fixture, name);
            unattended(
                &Shepherd::never_ready(),
                &fixture.tree,
                &mut fixture.state,
                &test_config_keeping(2),
            )
            .await
            .expect("rolls back");
        }

        assert!(
            fixture.tree.release(&staged).exists(),
            "the staged release outlives the newest two"
        );
    }

    /// fails if a deploy that lands leaves a staged sha behind. The record
    /// would then name a release the next stage reports as already built,
    /// for a head the branch has long moved past.
    #[tokio::test(start_paused = true)]
    async fn a_landed_deploy_clears_the_staged_sha() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        fixture.state.staged = Some(second.clone());

        let outcome = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
        assert_eq!(fixture.state.staged, None);
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("reads")
                .staged,
            None
        );
    }
}
