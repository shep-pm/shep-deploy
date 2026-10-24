//! Deciding whether a freshly reloaded sheep came up healthy.
//!
//! shep reports [`ProcStatus::Starting`] ("spawned, not yet ready") and
//! [`ProcStatus::Online`] ("running and, if configured, ready") for every
//! instance it supervises, and [`wait`] reads that verdict back through
//! [`Daemon::describe`] until either it holds or the deploy's grace period
//! runs out.
//!
//! # Why a status alone is not the verdict
//!
//! `Request::Reload` is an ACCEPTANCE, not a completion. shep answers it and
//! then runs `SpawnNew`, `AwaitReady`, `DrainOld`, `ReapOld` in its own actor
//! loop, so at the instant polling starts the OLD instance is still there and
//! still `Online`. A check that asks only "is any instance of this sheep
//! Online" therefore passes on the process that was already serving, before
//! the replacement has proved anything at all.
//!
//! It is worse than a race. When shep's own `AwaitReady` fails it
//! deliberately keeps the old instance serving, so the listing settles back
//! to exactly one `Online` entry: the old one. Measured against a real
//! shepherd, a release whose readiness probe could never pass looked like
//! this, one line per second:
//!
//! ```text
//! before  [(id 1, online,   pid 12835)]
//! t=1s    [(id 1, stopping, pid 12835), (id 2, starting, pid 13002)]
//! t=3s    [(id 1, online,   pid 12835)]      <- and forever after
//! ```
//!
//! A status-only check reports that deploy healthy on its first poll and on
//! every poll after it. Every rollback this crate can perform sits behind
//! that check, so getting it wrong disables the feature rather than
//! degrading it.
//!
//! # Generations
//!
//! [`Generation`] is the set of pids serving a sheep at some moment.
//! Verification captures one BEFORE the reload and then waits for the flock
//! to have turned over: every instance the shepherd reports must be running
//! under a pid that generation never had, and there has to be at least one.
//!
//! Pid, rather than [`ProcessInfo::id`] or `uptime_ms`. A pid is the
//! operating system's own answer to "is this a different process", which is
//! the exact question, and it needs no reasoning about shep's bookkeeping to
//! interpret. `id` would work today (the measurement above shows the
//! replacement getting a fresh one), but it is shep's numbering rather than
//! the machine's fact, and `uptime_ms` only ever supports an inference about
//! which process is which. The one thing pid cannot rule out is the kernel
//! recycling a pid onto the replacement within the same reload, which is not
//! worth defending against here.
//!
//! Every instance, not any: a sheep scaled to several instances reloads them
//! one at a time, and "any new instance is Online" would report success while
//! shep was still keeping an old process alive for the ones whose
//! `AwaitReady` failed. Waiting for a total turnover also absorbs the drain
//! window for free, since an old instance still draining is reported as
//! `Stopping` under its old pid and simply is not a turnover yet.

mod dwell;
mod generation;
#[cfg(test)]
mod test_support;

use std::time::Duration;

use shep_client::shep_core::protocol::ProcessInfo;
use shep_client::shep_core::status::ProcStatus;
use tokio::time::{Instant, sleep};

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::Verify;

pub use generation::Generation;

/// How long `Alive` watches a turned-over flock before believing it.
///
/// The spec's own wording for `alive` is "wait N seconds and confirm the
/// process is still running", and this is that N. It is a dwell after the
/// turnover rather than a window the turnover has to fit inside: how long a
/// reload takes is the app's business, and `crate::deploy` derives that from
/// the app, while how long a new release has to survive before anyone
/// believes in it is this crate's.
pub(crate) const DWELL: Duration = Duration::from_secs(10);

/// How often either mode asks the shepherd again.
///
/// One cadence for the crate: `crate::optin`'s cutover polls on this too,
/// because "how often do we ask the shepherd again" has one answer here.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// The most any wait on the shepherd may last, whatever the app config says.
///
/// An hour. Every budget in this crate is built from the app's
/// `listen_timeout` and `graceful_timeout`, and `UpDuration` accepts values up to
/// `u64::MAX` milliseconds, so an app config could ask this dog to watch a
/// reload for half a billion years, holding the tree's lock and the poll
/// loop with it, and a value large enough overflowed `Instant + Duration`
/// and took the dog down instead. An hour is past any reload that could
/// still succeed and short of any of that.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// `budget`, held under [`MAX_WAIT`].
///
/// Every budget derived from an app config goes through here before it is
/// added to a clock. Both callers say so at the site.
#[must_use]
pub(crate) fn bounded(budget: Duration) -> Duration {
    budget.min(MAX_WAIT)
}

/// Watch `sheep` until the flock has turned over, which it gets `budget` to
/// do, judging health the way `mode` says to against the generation that
/// was serving before the reload.
///
/// Both modes poll, and both wait for the same turnover: every instance the
/// shepherd reports running under a pid `before` never had. They differ only
/// in what they then accept, and in what they do afterwards.
///
/// `Probed` waits for the turned-over flock to be all
/// [`ProcStatus::Online`], then watches it for `dwell`, requiring the same
/// pids and every one `Online` at every look. The wait can only mean
/// anything for a sheep whose readiness is gated - a probe, or
/// `wait_ready` on the channel - since otherwise shep reports `Online` for
/// a process that merely has not died; `deploy` refuses a `Probed` target
/// with neither before it touches anything. The dwell is there because
/// `Online` is a verdict given once: a replacement that passes its probe
/// and dies moments later is respawned under a new pid with the reload
/// already committed, and one whose post-drain re-probe fails is demoted to
/// `Starting` under the same pid; the pid and the status are the only
/// things that say so. `dwell` is [`DWELL`] for most apps and longer for one
/// whose post-drain re-probe can take longer: see `deploy::reload::land`.
///
/// `Alive` accepts `Starting` as well, because a sheep with no gate is
/// exactly what it exists for, and then dwells: it sleeps [`DWELL`], then
/// polls for the same pids to still be there AND to have reached `Online`,
/// for as long as the turnover left of `budget` or one more [`DWELL`],
/// whichever is longer. So `Alive` can run past `budget` by up to three
/// dwells (the sleep, the floor on the deadline, and one `describe` that
/// starts just inside it), and that is the shape the spec asks for: "wait N
/// seconds and confirm the process is still running". A process that came up and died
/// inside the dwell has a different pid by then (shep restarts it) or none
/// at all, either of which fails. `WaitingRestart` is a fail throughout for
/// the same reason.
///
/// # Why `Alive` polls at all
///
/// It used to sleep one fixed window and sample once. That shape cannot tell
/// "the reload is still in flight" from "the release failed", and the
/// difference matters as soon as a wrong answer rolls back: a probeless app
/// takes shep's heuristic readiness path, which sleeps the app's whole
/// `listen_timeout` per instance, so a reload can legitimately outlive any
/// fixed window. Measured, a one-instance probeless app with
/// `listen_timeout = "15s"` had a perfectly good release given up on at 10
/// seconds, rolled back, and the rollback's own reload refused because the
/// first one was still running.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns - either mode can surface a
/// transient error mid-poll rather than only at the deadline.
pub async fn wait<D: Daemon>(
    daemon: &D,
    sheep: &str,
    mode: Verify,
    before: &Generation,
    budget: Duration,
    dwell: Duration,
) -> Result<bool, Error> {
    let accept = match mode {
        Verify::Probed => is_online,
        Verify::Alive => is_alive,
    };

    let started = Instant::now();
    let Some(settled) = turnover(daemon, sheep, before, accept, budget).await? else {
        return Ok(false);
    };

    match mode {
        Verify::Probed => dwell::probed(daemon, sheep, &settled, dwell).await,
        Verify::Alive => dwell::alive(daemon, sheep, &settled, started + budget).await,
    }
}

/// [`Daemon::describe`], retrying an answer that could clear on its own.
///
/// The judgement is [`Error::is_retryable`]'s, which the crate already applies
/// to `reload` over the same socket: a `Timeout` or an `Internal` is the
/// shepherd being busy, a `NotFound` is a question that will never have a
/// different answer.
///
/// `budget` bounds the retrying, so a shepherd that is genuinely gone still
/// fails rather than hanging.
///
/// It does NOT bound the asking, and that is deliberate rather than loose.
/// A caller's wall-clock can therefore exceed its own `budget` by one slow
/// answer, which round 7 of the founder's review raised. Kept, for two
/// reasons. The overrun is bounded without any help from here: every
/// `describe` goes through `shep_client`'s `Client::request`, which waits
/// `DEFAULT_DEADLINE` plus `DEADLINE_GRACE`, five seconds and two, the same
/// pair `crate::deploy` already names in four places.
/// And the obvious fix is worse than the problem: wrapping the call in a
/// timeout against what is left of the budget means the last poll of a
/// `turnover` gets almost no time and fails for that reason, which is how a
/// healthy release gets rolled back at the cost of a second live reload. That
/// is the exact failure the retry above this line was added to prevent.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns, once a failure is either fatal or
/// the budget is spent.
async fn describe_within<D: Daemon>(
    daemon: &D,
    sheep: &str,
    budget: Duration,
) -> Result<Vec<ProcessInfo>, Error> {
    let deadline = Instant::now() + budget;
    loop {
        match daemon.describe(sheep).await {
            Ok(flock) => return Ok(flock),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                // `budget` bounds the RETRYING, never the asking. The caller
                // wanted one describe and gets it whatever the clock says;
                // `turnover` in particular hands over whatever is left of its
                // own deadline and still expects the question put, because its
                // loop is what decides that time is up.
                //
                // The sleep is clamped so a retry cannot run past the
                // deadline, which is the part that was genuinely loose:
                // `POLL` is 100ms and a budget with less than that remaining
                // used to overshoot it.
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(err);
                }
                sleep(POLL.min(left)).await;
            }
        }
    }
}

/// Polls until `before` has been replaced by instances `accept` is happy
/// with, and answers with the generation that replaced it.
///
/// `None` is the deadline passing with no such turnover, which is the
/// failure both modes report.
///
/// # Errors
/// Whatever [`Daemon::describe`] returns, once a failure is either fatal or
/// the budget has run out. A retryable failure inside the budget is polled
/// through rather than reported: see the body for why one blip must not cost
/// a live reload.
async fn turnover<D: Daemon>(
    daemon: &D,
    sheep: &str,
    before: &Generation,
    accept: fn(&ProcessInfo) -> bool,
    budget: Duration,
) -> Result<Option<Generation>, Error> {
    let deadline = Instant::now() + budget;
    loop {
        // A transient answer is not a verdict. This polls roughly a hundred
        // and fifty times over a ten-to-twenty second budget, so a single
        // `Timeout` or `Internal` anywhere in that run used to abort
        // verification and send `land` down the rollback path: a second real
        // reload, under live traffic, for a release that was healthy.
        //
        // Same judgement `Error::is_retryable` already made for `reload` over
        // the same socket. Retried inside the budget rather than beyond it,
        // so a shepherd that is genuinely gone still fails at the deadline
        // instead of hanging.
        let flock = describe_within(
            daemon,
            sheep,
            deadline.saturating_duration_since(Instant::now()),
        )
        .await?;
        if before.has_turned_over(&flock, accept) {
            return Ok(Some(Generation::of_infos(
                &flock.iter().collect::<Vec<_>>(),
            )));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(POLL).await;
    }
}

/// Whether one instance has passed its readiness probe, for [`Verify::Probed`].
fn is_online(info: &ProcessInfo) -> bool {
    info.status == ProcStatus::Online
}

/// Whether one instance still counts as "running" for [`Verify::Alive`].
///
/// Shared with `crate::optin`'s cutover rather than copied into it: the
/// cutover's accept predicate IS this one, and two spellings of it would
/// drift apart with nothing to notice.
pub(crate) fn is_alive(info: &ProcessInfo) -> bool {
    matches!(info.status, ProcStatus::Starting | ProcStatus::Online)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use shep_client::shep_core::status::ProcStatus;

    use super::*;
    use test_support::{Blips, Listings, instance, serving, serving_all};

    /// fails if one transient `describe` failure costs a live reload.
    ///
    /// `turnover` polls roughly a hundred and fifty times over a ten-second
    /// budget. Propagating any one of those used to make `land` report
    /// `Landed::Failed`, which rolls back: `current` is put back and the sheep
    /// is reloaded a SECOND time, under live traffic, for a release that was
    /// healthy and one poll from Online.
    ///
    /// The error here is `Timeout`, which `Error::is_retryable` already
    /// classifies as transient for `reload` over the same socket. A fatal code
    /// must still fail, which `only_a_failure_that_could_clear_is_retried` in
    /// `crate::error` pins from the other side.
    #[tokio::test]
    async fn a_transient_describe_failure_does_not_fail_verification() {
        let before = serving(111);
        let daemon = Blips {
            calls: Cell::new(0),
            after: vec![instance(0, ProcStatus::Online, 222)],
        };

        let seen = turnover(&daemon, "web", &before, is_online, Duration::from_secs(10))
            .await
            .expect("a retryable blip must not end verification");

        assert!(
            seen.is_some(),
            "the turnover after the blip must still be seen"
        );
        assert!(daemon.calls.get() >= 2, "it must have asked again");
    }

    /// fails if the process that was already serving passes for the one the
    /// deploy just installed. This is the shape a real shepherd settles into
    /// when its own `AwaitReady` fails: it keeps the old instance, so the
    /// listing is one `Online` entry under the old pid, forever. A check
    /// that asks only about status reports that deploy healthy and every
    /// rollback in this crate becomes unreachable.
    #[tokio::test(start_paused = true)]
    async fn the_old_instance_still_online_is_not_a_new_release() {
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 12835)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a reload that shep accepted but never completed is treated
    /// as verified. `Request::Reload` is an acceptance, not a completion, so
    /// the first poll of a perfectly healthy deploy sees exactly this: the
    /// old instance, alone and Online, because the replacement has not been
    /// spawned yet.
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_has_not_happened_yet_is_not_success() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a replacement that reaches Online is not treated as success.
    /// The listing walks the real sequence: the old instance alone, then the
    /// two overlapping while the old drains, then the new one alone.
    #[tokio::test(start_paused = true)]
    async fn a_new_instance_reaching_online_is_success() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![
                instance(1, ProcStatus::Stopping, 12835),
                instance(2, ProcStatus::Starting, 13002),
            ],
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Probed,
                &serving(12835),
                Duration::from_secs(5),
                DWELL
            )
            .await
            .unwrap()
        );
    }

    /// fails if a draining old instance is enough to call the deploy done.
    /// During a reload the listing carries both, and the new one can be
    /// Online while the old one is still Stopping - a turnover is not
    /// finished until the old process is gone.
    #[tokio::test(start_paused = true)]
    async fn a_draining_old_instance_is_not_a_finished_turnover() {
        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Stopping, 12835),
            instance(2, ProcStatus::Online, 13002),
        ]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if Starting is mistaken for success under `Probed`.
    /// `ProcStatus::Starting` means "spawned, not yet ready" and `Online`
    /// means "running and, if configured, ready", so `Online` is the only
    /// status that means the probe passed.
    #[tokio::test(start_paused = true)]
    async fn a_new_instance_that_never_leaves_starting_is_not_success() {
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::Starting, 13002)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a flock that came back SMALLER is accepted as a turnover.
    ///
    /// "All new" is true of one new instance, so a two-instance sheep that
    /// loses a replica during the reload turns over on the survivor alone.
    /// `settled` then becomes that single pid and every later check compares
    /// the survivor against itself, including the dwell's own count. A sheep
    /// at half strength verified, and both modes believed it.
    ///
    /// Pinned at the turnover rather than the dwell because `Verify::Probed`
    /// fails on a short flock before any dwell begins.
    #[tokio::test(start_paused = true)]
    async fn a_turnover_that_lost_a_replica_is_not_a_turnover() {
        // Two before, one after, and that one is new and healthy.
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 200)]]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &before,
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();

        assert!(!ok, "one replacement for two instances is not a turnover");
    }

    /// fails if an instance with no pid at all is treated as a new one.
    /// `WaitingRestart` covers the status side of the same case; this
    /// covers the pid side, which is what `Generation::is_new` actually
    /// asks. A deploy verified against an instance with no process has been
    /// verified against nothing.
    #[tokio::test(start_paused = true)]
    async fn an_instance_with_no_pid_is_not_a_new_generation() {
        let daemon = Listings::new(vec![vec![
            ProcessInfo::builder(2, "web", ProcStatus::Online).build(),
        ]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a sheep the shepherd cannot find (an empty `describe`
    /// result) is ever treated as verified. Both modes must fail closed
    /// here: there is nothing to have come up healthy.
    #[tokio::test(start_paused = true)]
    async fn an_empty_describe_is_failure_in_both_modes() {
        let empty = Listings::new(Vec::new());
        assert!(
            !wait(
                &empty,
                "web",
                Verify::Probed,
                &serving(12835),
                Duration::from_millis(50),
                DWELL
            )
            .await
            .unwrap()
        );
        let empty = Listings::new(Vec::new());
        assert!(
            !wait(
                &empty,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_millis(50),
                DWELL
            )
            .await
            .unwrap()
        );
    }

    /// fails if the fixture stops repeating its last listing once its
    /// sequence is exhausted, whether by panicking or by returning an empty
    /// `Vec`. `Probed` polls every 100ms, so a 350ms window against a
    /// two-element sequence outlives it several times over; every one of
    /// those extra polls must keep reading the last listing rather than
    /// running off the end.
    #[tokio::test(start_paused = true)]
    async fn the_fixture_repeats_its_last_listing_past_exhaustion() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_millis(350),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a replica that was between restarts when the generation was
    /// captured stops the reload from ever being seen as complete.
    ///
    /// `WaitingRestart` has no pid, so it was missing from the pid set and
    /// present in the shepherd's listing. `has_turned_over` compared the two
    /// counts, found two against three forever, and rolled back a flock that
    /// had come back whole. Three rows before, three new pids after, must be
    /// a turnover.
    #[tokio::test(start_paused = true)]
    async fn a_replica_between_restarts_before_the_reload_still_counts() {
        let capture = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 100),
            instance(2, ProcStatus::Online, 101),
            ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart).build(),
        ]]);
        let before = Generation::of(&capture, "web").await.unwrap();

        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 200),
            instance(2, ProcStatus::Online, 201),
            instance(3, ProcStatus::Online, 202),
        ]]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Probed,
                &before,
                Duration::from_secs(5),
                DWELL
            )
            .await
            .unwrap(),
            "three rows replaced by three new pids is a turnover"
        );
    }

    /// fails if a budget past [`MAX_WAIT`] reaches a clock. Added to a
    /// `tokio::time::Instant`, `Duration::MAX` panics, and an
    /// app config can ask for any `listen_timeout` up to `u64::MAX`
    /// milliseconds.
    #[test]
    fn a_budget_is_held_under_the_ceiling() {
        assert_eq!(bounded(Duration::MAX), MAX_WAIT);
        assert_eq!(bounded(Duration::from_secs(5)), Duration::from_secs(5));
        // And the ceiling itself is something a clock can hold.
        let _ = Instant::now() + MAX_WAIT;
    }
}
