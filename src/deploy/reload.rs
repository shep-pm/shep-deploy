//! The reload a deploy asks of the shepherd, and the verdict on it.

use std::path::Path;
use std::time::Duration;

use tokio::time::{Instant, sleep};

use shep_client::RequestError;
use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::Verify;
use crate::verify;
use crate::verify::Generation;
use crate::window::{budget, dwell_for};

/// What became of a release between the swap and the verdict.
///
/// The distinction the whole enum exists for is whether the shepherd ever
/// ACCEPTED the reload, because that is what decides how much has to be
/// undone. Before it, the swap is the only thing that moved and putting
/// `current` back restores the machine exactly. After it, a process may be
/// running the new release, and a symlink moving back does not move a
/// running process.
///
/// That question is NOT the same as whether the reload request returned an
/// error, and reading it off `Result::is_err` was wrong in the direction
/// that leaves a machine split: see [`never_reached_the_shepherd`]. Only a
/// failure that is known to have happened before the shepherd could act
/// counts as [`Self::NotStarted`].
pub(super) enum Landed {
    /// The flock turned over and the new release is serving.
    Verified,
    /// The reload was never issued at all, so nothing was started on the
    /// new release.
    NotStarted(Error),
    /// The reload was issued, and the release did not come up.
    NotVerified {
        /// What to tell an operator, measured rather than budgeted.
        why: String,
        /// How long the rollback's own reload may keep retrying.
        patience: Duration,
    },
    /// The reload was issued, and something failed after it.
    Failed {
        /// What failed.
        source: Error,
        /// As [`Self::NotVerified::patience`].
        patience: Duration,
    },
}

/// Reloads `sheep` onto the release that has just been swapped in, and
/// waits for the verdict `mode` asks for.
///
/// Every failure a deploy can meet after the swap AND might have to undo
/// happens here, and that is the point rather than tidiness. This function
/// cannot return an error: each failure is a [`Landed`] variant, so the
/// caller's `match` is exhaustive over "what has to be undone" instead of
/// over "what went wrong", and a step added here later cannot quietly
/// escape the rollback decision the way `Generation::of` did - one `?`
/// outside the boundary left `current` on a release nothing had verified,
/// `deploy.toml` on the old one, no reload ever sent, and any later restart
/// bringing the sheep up on the unverified release.
///
/// One fallible step past the swap is deliberately NOT here: writing the
/// record after a successful verify. It has nothing to undo, because by
/// then the new release is up and serving. `deploy`'s own `Verified` arm
/// says so where that write is.
///
/// The generation is captured before the reload, and both readings of it
/// matter: [`crate::verify::wait`] needs to know which processes were
/// already serving, and [`budget`] needs to know how many of them there
/// are.
pub(super) async fn land<D: Daemon>(
    daemon: &D,
    sheep: &str,
    app: &AppConfig,
    mode: Verify,
) -> Landed {
    let before = match Generation::of(daemon, sheep).await {
        Ok(before) => before,
        Err(source) => return Landed::NotStarted(source),
    };
    let patience = budget(app, before.instances());

    if let Err(source) = daemon.reload(sheep).await {
        return if never_reached_the_shepherd(&source) {
            Landed::NotStarted(source)
        } else {
            Landed::Failed { source, patience }
        };
    }
    // After the RPC returns, so "{n}s after the reload" is measured from
    // the reload rather than from a request before it.
    let reloaded_at = Instant::now();

    match verify::wait(daemon, sheep, mode, &before, patience, dwell_for(app)).await {
        Ok(true) => Landed::Verified,
        // What actually elapsed, not the budget. An `Alive` verdict takes
        // its turnover wait PLUS the dwell, so quoting the budget
        // understated the wait every time that mode failed.
        Ok(false) => Landed::NotVerified {
            why: format!(
                "it did not come up and stay up, {}s after the reload",
                reloaded_at.elapsed().as_secs()
            ),
            patience,
        },
        Err(source) => Landed::Failed { source, patience },
    }
}

/// Whether a failed reload request definitely never started a swap.
///
/// An allowlist, not a denylist, and that direction is the whole of the
/// decision: being wrong here in the "nothing started" direction leaves a
/// process running the new release with `current` pointed back at the old
/// one and no reload ever sent, which is the split state
/// [`Error::Split`] exists to name. Being wrong the other way costs one
/// needless reload of a healthy sheep. So anything not known to have failed
/// before the shepherd could act is treated as though it might have acted.
///
/// Two shapes qualify, both from [`RequestError`]'s own documented meaning:
///
/// - `Rpc`, which is "the daemon accepted the request and answered it with a
///   structured error". shep answers a reload with an ACCEPTANCE before it
///   spawns anything (`handle_reload` refuses on the presence of the map key
///   first), so an error answer means no swap began. This is the case
///   `ReloadInFlight` arrives as.
/// - `Wire`, which is the request body failing to encode. It never left this
///   process.
///
/// Everything else is ambiguous and is treated as an acceptance that may
/// have happened. `Timeout` is "no reply within the deadline plus
/// `DEADLINE_GRACE`", five seconds and two, which is a real window in which
/// the shepherd can have accepted and begun spawning from the new `current`.
/// `Closed` is the connection dropping before the reply arrived, which says
/// nothing about whether the request was acted on. [`Error::Protocol`] means
/// the shepherd answered SOMETHING, so it processed the request, and a
/// daemon answering the wrong variant tells us nothing about what it did.
/// `RequestError` is `#[non_exhaustive]`, so a variant added later lands in
/// the same fail-safe direction rather than in this list.
///
/// [`Error::is_retryable`] draws its own line in the same place for the same
/// reason, and the two disagreeing is what let this through: the rollback's
/// reload already treated `Timeout` as "might yet clear" while the deploy's
/// reload treated it as "never happened".
fn never_reached_the_shepherd(err: &Error) -> bool {
    matches!(
        err,
        Error::Request(RequestError::Rpc(_) | RequestError::Wire(_))
    )
}

/// How long to leave between attempts at a reload the shepherd would not
/// take.
const RETRY_EVERY: Duration = Duration::from_millis(500);

/// Reloads `sheep`, retrying a failure that could clear until `patience`
/// runs out.
///
/// The refusal this exists for is transient and self-clearing: the shepherd
/// will not start a reload while another is in flight, and the one in flight
/// is precisely what verification just gave up waiting for. Retrying is
/// therefore the difference between a rollback that lands a few seconds late
/// and one that cannot happen at all. What is retried and what is not is
/// [`Error::is_retryable`]'s decision.
///
/// # Errors
/// The failure, at once if it could never clear, or the last one after
/// `patience` has run out.
pub(super) async fn reload_until<D: Daemon>(
    daemon: &D,
    sheep: &str,
    patience: Duration,
) -> Result<(), Error> {
    let deadline = Instant::now() + patience;
    loop {
        match daemon.reload(sheep).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !err.is_retryable() || Instant::now() >= deadline {
                    return Err(err);
                }
                sleep(RETRY_EVERY).await;
            }
        }
    }
}

/// A release path as it reads in a message, or "nothing" when `current`
/// has never been pointed anywhere - the zero-configuration case.
pub(super) fn named(release: Option<&Path>) -> String {
    release.map_or_else(|| "nothing".to_owned(), sha_of)
}

/// The sha a release path names, which is its last component - see
/// [`crate::paths::Tree::release`]. Falls back to the whole path when that component is
/// not readable as a string, so an odd path degrades to a longer message
/// rather than to no message.
pub(super) fn sha_of(release: &Path) -> String {
    release.file_name().map_or_else(
        || release.display().to_string(),
        |sha| sha.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use core::error::Error as _;
    use std::time::Duration;

    use shep_client::shep_core::protocol::RpcError;
    use shep_client::shep_core::protocol::RpcErrorCode;
    use shep_client::shep_core::protocol::wire::WireError;

    use crate::deploy::test_support::{Shepherd, commit_on_origin, fixture_with_previous_release};
    use crate::deploy::{Outcome, deploy};
    use crate::error::Error;
    use crate::fixtures;
    use crate::swap;

    use super::*;

    /// fails if the process that was already serving passes for the
    /// release this deploy installed. Measured against a real shepherd:
    /// when its own `AwaitReady` fails, shep keeps the old instance, so
    /// `describe` settles back to one `Online` entry under the pid that was
    /// already there. A verification that reads status alone calls that
    /// deploy healthy, and every rollback in this crate becomes
    /// unreachable - which is what it was, for two review rounds.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_keeps_the_old_instance_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::keeps_the_old_instance(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if the reload window is sized from the Flockfile's instance
    /// count rather than the flock's. `AppConfig::instances` is what the
    /// file ASKS for; `shep stock <sheep> <n>` changes what is running
    /// without touching any file this crate reads, and a reload costs one
    /// swap per running instance.
    ///
    /// The numbers here are the reviewer's reproduction, scaled to the
    /// fixture: one instance in the file, two actually running, and a
    /// replacement that takes about twenty seconds to appear. Sized off the
    /// file that is a sixteen-second budget and a healthy release is rolled
    /// back; sized off the flock it is thirty-two and the deploy stands.
    #[tokio::test(start_paused = true)]
    async fn the_window_follows_the_running_flock_not_the_flockfile() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        // 200 polls at 100ms apart, so the turnover lands near 20s: inside
        // two instances' budget and outside one's.
        let daemon = Shepherd::ready_after(200).running(2);

        let outcome = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if a rollback gives up the first time the shepherd says it is
    /// busy. That refusal is transient and self-clearing - it means the
    /// reload verification just gave up on is still running - so retrying
    /// is the difference between a rollback that lands a few seconds late
    /// and one that cannot happen at all.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_retries_a_reload_the_shepherd_is_too_busy_for() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");

        let daemon = Shepherd::busy_for(3);
        let outcome = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        // The deploy's own reload, three refusals, then the one that took.
        assert_eq!(daemon.attempt_count(), 5);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if the reason a rollback happened quotes the budget instead of
    /// what elapsed. An `Alive` verdict takes its turnover wait PLUS the
    /// dwell, so a release that comes up and then will not stay up is
    /// rejected at a moment the budget never names - here ten seconds of
    /// dwell against a sixteen-second budget. An operator reading the
    /// budget back is being told how long the deploy was allowed to take,
    /// not how long it took.
    #[tokio::test(start_paused = true)]
    async fn the_reason_quotes_what_elapsed_not_what_was_allowed() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.verify = crate::state::Verify::Alive;
        commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::flapping(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        let Outcome::RolledBack { why, .. } = outcome else {
            panic!("a release that will not stay up must roll back: {outcome:?}");
        };
        // The dwell alone: the turnover was immediate, and the budget for
        // this app is 3 + 8 + 5 = 16s.
        assert!(why.contains("10s"), "{why}");
        assert!(!why.contains("16s"), "{why}");
    }

    /// fails if a reload the shepherd can never accept is retried anyway,
    /// or dressed up as a split state. A `NotFound` means no sheep of that
    /// name is registered: asking again cannot change it, so the retry
    /// burned the whole budget, and the `Split` it then reported claimed a
    /// running process there was none of and prescribed a `shep reload`
    /// that fails identically.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_can_never_succeed_is_not_retried_or_called_a_split() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::unregistered();

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("the rollback cannot reload");

        assert!(!matches!(err, Error::Split { .. }), "{err:?}");
        assert!(matches!(err, Error::RollbackFailed { .. }), "{err:?}");
        assert!(err.to_string().contains("NotFound"), "{err}");
        // The deploy's own reload and exactly one rollback attempt.
        assert_eq!(daemon.attempt_count(), 2);
    }

    /// fails if a reload whose REPLY was lost is treated as one that never
    /// happened. `Timeout` is "no reply within the deadline plus
    /// `DEADLINE_GRACE`" - five seconds and two - and shep answers a reload
    /// as an acceptance precisely because a real one outlives that. So the
    /// shepherd can have accepted it and begun spawning from the new
    /// `current` before the client gave up waiting.
    ///
    /// Sent down the "nothing was started" path, that becomes a genuinely
    /// split machine reported as a bare transport error: `current` and
    /// `deploy.toml` on the old release, the flock running the new one, and
    /// no reload ever sent onto the old release.
    #[tokio::test(start_paused = true)]
    async fn a_reload_whose_reply_is_lost_is_rolled_back() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        // The deploy's own reply is lost; the rollback's gets through.
        let daemon = Shepherd::losing_replies(1);

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no reply came back");

        // Rolled back, not shrugged off: the deploy failed, and the error
        // says what is live again with the timeout still underneath it.
        assert!(matches!(err, Error::RolledBack { .. }), "{err:?}");
        assert!(
            matches!(
                err.source()
                    .and_then(<dyn core::error::Error>::downcast_ref),
                Some(Error::Request(RequestError::Timeout { .. }))
            ),
            "{err:?}"
        );
        // The deploy's own reload, and the rollback's onto the old release.
        // The second is the one the "nothing started" path never sent.
        assert_eq!(daemon.attempt_count(), 2);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if the "did it start" question stops being read off the error
    /// shape. Only a failure that is known to have happened before the
    /// shepherd could act counts, because being wrong that way leaves a
    /// split machine while being wrong the other way costs one needless
    /// reload of a healthy sheep.
    #[test]
    fn only_a_definite_refusal_counts_as_never_started() {
        let rpc = Error::Request(RequestError::Rpc(RpcError {
            code: RpcErrorCode::Internal,
            message: "web is already being reloaded".to_owned(),
            daemon_version: None,
        }));
        assert!(never_reached_the_shepherd(&rpc));

        // The other half of the allowlist, and it had no test at all until
        // round 9 of the founder's review deleted the `Wire` arm and watched
        // all 269 tests stay green. A frame this process could not encode
        // never left it, so the shepherd cannot have acted on it.
        assert!(never_reached_the_shepherd(&Error::Request(
            RequestError::Wire(WireError::FrameTooLarge(1 << 30))
        )));

        // Ambiguous: the shepherd may have accepted and acted.
        assert!(!never_reached_the_shepherd(&Error::Request(
            RequestError::Timeout {
                after: Duration::from_secs(7)
            }
        )));
        assert!(!never_reached_the_shepherd(&Error::Request(
            RequestError::Closed
        )));
        // It answered something, so it processed the request.
        assert!(!never_reached_the_shepherd(&Error::Protocol(
            "a Flock in answer to Reload".to_owned()
        )));
    }

    /// fails if a refusal of the deploy's OWN reload is treated as though
    /// the release had been started. An operator running `shep reload web`
    /// by hand during a poll-loop deploy produces exactly this: the
    /// shepherd refuses a second reload while one is in flight, so nothing
    /// was ever spawned on the new release.
    ///
    /// What the post-reload rollback did with it was swap back, correct the
    /// record, retry the reload for the whole budget, get refused
    /// throughout, and report `Error::Split` - the variant reserved for a
    /// state this crate cannot repair - about a machine that was entirely
    /// consistent, telling the operator to finish a rollback that had
    /// already finished.
    #[tokio::test(start_paused = true)]
    async fn a_refused_reload_is_not_a_split_state() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::too_busy_to_start();

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("the shepherd would not take the reload");

        assert!(!matches!(err, Error::Split { .. }), "{err:?}");
        assert!(err.to_string().contains("already being reloaded"), "{err}");
        // One attempt, not a budget's worth of retries: there is nothing to
        // retry towards when nothing was started.
        assert_eq!(daemon.attempt_count(), 1);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if an error arriving after the reload gets a lesser rollback
    /// than a failed verification does. `land` reloads before it verifies,
    /// so a `describe` that fails transiently - which `verify::wait`'s own
    /// doc anticipates - arrives with shep already running the new release
    /// and the old instance already drained. A swap without a reload there
    /// leaves the daemon on the new code while both signals an operator
    /// would check name the old one.
    #[tokio::test]
    async fn a_verify_error_after_the_reload_still_reloads_the_rollback() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::describe_fails();

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("describe fails");

        // The deploy still failed, so this is still an error - but it
        // names the rollback, and the shepherd's own complaint is still
        // reachable underneath it.
        assert!(matches!(err, Error::RolledBack { .. }), "{err:?}");
        assert!(
            err.to_string().contains("the shepherd stopped answering"),
            "{err}"
        );
        assert!(
            matches!(
                err.source().and_then(|s| s.downcast_ref()),
                Some(Error::Protocol(_))
            ),
            "{err:?}"
        );
        assert_eq!(
            daemon.reload_count(),
            2,
            "one reload onto the new release, one onto the old"
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    }
}
