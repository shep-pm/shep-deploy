//! What `wait` does once the flock has turned over: watch it for a while
//! before believing it.

use std::time::Duration;

use tokio::time::{Instant, sleep};

use super::{DWELL, Generation, POLL, describe_within, is_alive, is_online};
use crate::daemon::Daemon;
use crate::error::Error;

/// Whether `settled` stays whole and `Online` for the whole of `dwell`.
pub(super) async fn probed<D: Daemon>(
    daemon: &D,
    sheep: &str,
    settled: &Generation,
    dwell: Duration,
) -> Result<bool, Error> {
    // `Online` is a one-time verdict. Read out of shep-daemon on
    // 2026-09-04: once a probed replacement has answered its probe and
    // the old instance has drained, the reload is committed and nothing
    // re-checks it. A replacement that dies two seconds later is
    // restarted under a NEW pid, re-enters `Online` when its probe
    // passes again (or on the ordinary path even when it does not), and
    // no event retracts the reload. A replacement whose post-drain
    // re-probe fails is DEMOTED to `Starting` under the same pid and
    // left there. So a probed release used to be recorded deployed on
    // a process that was already gone. The dwell watches for `dwell`,
    // which the caller sizes to the app (see `deploy::reload::land`): the same
    // pids, every one of them `Online`, at every look. A pid that
    // changed or a status that fell is a verdict at once.
    let deadline = Instant::now() + dwell;
    loop {
        sleep(POLL).await;
        let flock = describe_within(daemon, sheep, dwell).await?;
        let still = u32::try_from(flock.len()).is_ok_and(|n| n == settled.instances())
            && flock
                .iter()
                .all(|info| settled.holds(info) && is_online(info));
        if !still {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Ok(true);
        }
    }
}

/// Whether `settled` stays whole after [`DWELL`] and reaches `Online` before
/// `give_up`, which is never earlier than one more [`DWELL`] from now.
pub(super) async fn alive<D: Daemon>(
    daemon: &D,
    sheep: &str,
    settled: &Generation,
    give_up: Instant,
) -> Result<bool, Error> {
    sleep(DWELL).await;
    // How long the replacement gets to reach `Online` after the dwell: what
    // is left of the budget, and never less than one more dwell.
    //
    // The budget, because the budget is what knows the app. A probeless
    // instance is marked `Online` by shep's heuristic path when its
    // `listen_timeout` elapses, and `budget` is built from that field. A
    // fixed second dwell here capped the whole wait at twenty seconds
    // whatever the app declared, so `verify = "alive"` on an app with
    // `listen_timeout = "30s"` was rolled back, healthy, every time, at the
    // cost of a second live reload. The README's own example uses fifteen
    // seconds, which is why it looked fine.
    let deadline = give_up.max(Instant::now() + DWELL);
    // Retried like the one in `turnover`, and for the same reason. Retrying
    // there and not here left `Verify::Alive` with exactly the bug the retry
    // was added to fix: one transient answer after the dwell propagates,
    // `land` reads it as a failure, and a healthy release is rolled back at
    // the cost of a second live reload. Found on 2026-08-28, in review of the
    // commit that added the first retry.
    // Polled for `Online`, not sampled once for "still alive", and the
    // difference is what shep 0.1.10 made necessary.
    //
    // A probed app now reloads serially, so by the time readiness resolves the
    // old instance has drained and there is nothing to abort back to. Rather
    // than mark a replacement `Online` it knows never became ready, shep
    // leaves it `Starting` and fires `ReloadAbandoned`. Its own comment gives
    // the reason: answering `online` for an instance that never came up "is
    // how a broken release gets recorded as deployed". A replacement stuck
    // that way has no liveness loop either, because those are armed at
    // `went_online`.
    //
    // Accepting `Starting` here threw that away and recorded exactly what
    // shep had just refused to claim.
    //
    // Requiring `Online` at a single sample was the first attempt and is
    // wrong. It would fail a healthy probeless app whose `listen_timeout`
    // outlasts the dwell, which is not hypothetical: this crate's own README
    // measures a one-instance probeless app at `listen_timeout = "15s"` being
    // given up on at ten seconds, and calls that the bug the polling turnover
    // above exists to prevent. Reintroducing it here would be the same
    // mistake one phase later.
    //
    // So the question is not "is it Starting" but "is it STILL Starting after
    // long enough". A slow app reaches `Online` on its own; an abandoned
    // replacement never does, because nothing is left to move it.
    loop {
        let flock = describe_within(daemon, sheep, DWELL).await?;
        // The COUNT as well as the pids. `all` over a listing that shrank is
        // vacuously happy: with `settled` holding two, a listing of one
        // `Online` pid from that generation satisfies every element and the
        // sibling that vanished goes unmentioned. `alive_rejects_a_flock_that_empties_during_the_dwell`
        // only pins the all-gone case.
        let settled_and_ready = u32::try_from(flock.len()).is_ok_and(|n| n == settled.instances())
            && flock
                .iter()
                .all(|info| settled.holds(info) && is_online(info));
        if settled_and_ready {
            return Ok(true);
        }
        // A pid that changed, or an instance that died, is a verdict rather
        // than something more waiting can fix.
        if flock.is_empty()
            || !flock
                .iter()
                .all(|info| settled.holds(info) && is_alive(info))
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use shep_client::shep_core::status::ProcStatus;

    use super::*;
    use crate::state::Verify;
    use crate::verify::test_support::{Listings, instance, serving, serving_all};
    use crate::verify::wait;

    /// fails if `Probed` believes a replacement that passed its probe and
    /// died moments later. shep restarts it under a new pid and the reload
    /// stays committed, so the only evidence is the pid at the dwell.
    #[tokio::test(start_paused = true)]
    async fn probed_rejects_a_replacement_that_dies_after_its_probe_passed() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Online, 13002)],
            // Respawned by the dwell: same id, new pid, Online again.
            vec![instance(2, ProcStatus::Online, 13099)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_secs(5),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok, "a new pid at the dwell is a process that died");
    }

    /// fails if `Probed` believes a replacement shep demoted. A failed
    /// post-drain re-probe puts the replacement back to `Starting` under
    /// the same pid and leaves it there; status is the only evidence.
    #[tokio::test(start_paused = true)]
    async fn probed_rejects_a_replacement_demoted_after_the_drain() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Online, 13002)],
            vec![instance(2, ProcStatus::Starting, 13002)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_secs(5),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok, "demoted out of Online is not deployed");
    }

    /// fails if the probed dwell stops at ten seconds when the caller asked
    /// for more. A `reuse_port` app's post-drain re-probe can demote it any
    /// time up to its `listen_timeout`, which `window::dwell_for` passes in
    /// here; a demotion fifteen seconds after the turnover has to be seen.
    #[tokio::test(start_paused = true)]
    async fn probed_watches_for_the_whole_dwell_it_was_given() {
        // Turnover, then `Online` for 150 polls (fifteen seconds at `POLL`),
        // then demoted for good.
        let demoted_late = || {
            let mut sequence = vec![vec![instance(2, ProcStatus::Online, 13002)]; 151];
            sequence.push(vec![instance(2, ProcStatus::Starting, 13002)]);
            Listings::new(sequence)
        };

        let ok = wait(
            &demoted_late(),
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(!ok, "a demotion inside a thirty-second dwell is seen");

        let ok = wait(
            &demoted_late(),
            "web",
            Verify::Probed,
            &serving(12835),
            Duration::from_secs(5),
            DWELL,
        )
        .await
        .unwrap();
        assert!(ok, "the plain dwell has already returned by then");
    }

    /// fails if `Verify::Alive` starts demanding a prompt probe. Alive is the
    /// deliberate, visible downgrade for a sheep with no probe: a new process
    /// that comes up and stays up is enough, and it is given the whole dwell
    /// to get there where `Probed` wants `Online` at the turnover.
    ///
    /// The fake reaches `Online` on its second answer, which it did not have
    /// to before. A probeless app takes shep's heuristic path and is marked
    /// `Online` at its `listen_timeout`, so a listing that says `Starting`
    /// forever is not a slow healthy app: since shep 0.1.10 it is the shape of
    /// a reload shep abandoned, and `alive_rejects_a_replacement_shep_gave_up_on`
    /// below is what pins that.
    #[tokio::test(start_paused = true)]
    async fn alive_accepts_a_new_process_that_is_still_running() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
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

    /// fails if `Verify::Alive` accepts the process that was already there.
    /// Alive is a downgrade on what "healthy" means, not on whether a
    /// deploy happened at all: a reload that put nothing new in place has
    /// not been verified by either mode.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_the_old_process_still_running() {
        let daemon = Listings::new(vec![vec![instance(1, ProcStatus::Online, 12835)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if a new instance that already died and is backed off waiting
    /// to be restarted counts as alive. This is the reject side of
    /// `is_alive`, and it is the failure `Alive` exists to catch: the
    /// process was spawned, so its pid is new, and it did not survive the
    /// window.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_an_instance_waiting_to_be_restarted() {
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::WaitingRestart, 13002)]]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_millis(50),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if `Alive` gives up on a reload that is still in flight. A
    /// probeless app takes shep's heuristic readiness path, which sleeps
    /// the app's whole `listen_timeout` per instance, so the old instance
    /// is all there is to see for as long as that takes. Sampling once at a
    /// fixed ten seconds called a perfectly good release dead, rolled it
    /// back, and then could not reload because the first reload was still
    /// running.
    #[tokio::test(start_paused = true)]
    async fn alive_waits_out_a_reload_that_is_still_in_flight() {
        let daemon = Listings::new(vec![
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(1, ProcStatus::Online, 12835)],
            vec![instance(2, ProcStatus::Starting, 13002)],
            // Then ready, which is what the heuristic path does at
            // `listen_timeout`. Without this the fake describes a replacement
            // that never becomes ready, which is a different case entirely and
            // has its own test below.
            vec![instance(2, ProcStatus::Online, 13002)],
        ]);
        assert!(
            wait(
                &daemon,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_secs(120),
                DWELL
            )
            .await
            .unwrap()
        );
    }

    /// fails if the dwell stops counting how many instances are left.
    ///
    /// `all` over a listing that shrank is vacuously happy: with `settled`
    /// holding two pids, a listing of ONE `Online` pid from that generation
    /// satisfies every element, and the sibling that vanished goes unmentioned.
    /// A scaled sheep that comes back at half strength is a release that did
    /// not deploy, and the dwell is the only thing looking.
    ///
    /// `alive_rejects_a_flock_that_empties_during_the_dwell` pins the all-gone
    /// case, which is the easy half.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_flock_that_came_back_smaller() {
        let daemon = Listings::new(vec![
            vec![
                instance(1, ProcStatus::Online, 200),
                instance(2, ProcStatus::Online, 201),
            ],
            // One of the two is gone by the dwell. The survivor is healthy and
            // from the right generation, which is what let this pass without a
            // count.
            vec![instance(1, ProcStatus::Online, 200)],
        ]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &before,
            Duration::from_secs(120),
            DWELL,
        )
        .await
        .unwrap();

        assert!(!ok, "half a flock is not a deployed release");
    }

    /// fails if `Alive` believes a replacement shep gave up on.
    ///
    /// shep 0.1.10 reloads a probed app serially, so when readiness resolves
    /// the old instance has already drained and there is nothing to abort back
    /// to. Rather than claim `Online` for a replacement it knows never became
    /// ready, shep leaves it `Starting` and fires `ReloadAbandoned`. Its own
    /// comment says why: answering `online` for an instance that never came up
    /// "is how a broken release gets recorded as deployed". Such an instance
    /// has no liveness loop either, since those are armed at `went_online`.
    ///
    /// Accepting that as alive threw the signal away and recorded the thing
    /// shep had just refused to claim. Caught against a real shepherd by the
    /// integration tier, which is the only place it could be caught: no fake
    /// in `verify` knew `Starting` had acquired a meaning.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_replacement_shep_gave_up_on() {
        // Starting forever, which is exactly what an abandoned reload leaves.
        let daemon = Listings::new(vec![vec![instance(2, ProcStatus::Starting, 13002)]]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
            DWELL,
        )
        .await
        .unwrap();

        assert!(
            !ok,
            "a replacement left at `starting` never came up, whatever verify mode asked"
        );
    }

    /// fails if `Alive` believes a release that came up and then died. The
    /// dwell is the whole of what `alive` promises - "wait N seconds and
    /// confirm the process is still running" - and a single sample taken at
    /// the moment of turnover confirms nothing. shep restarts the corpse,
    /// so what the dwell sees is a pid that is new again.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_process_that_dies_during_the_dwell() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            vec![instance(2, ProcStatus::Starting, 13456)],
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if the dwell settles for SOME instance still being the one that
    /// came up, rather than all of them.
    ///
    /// The module argues this for the turnover phase and
    /// `a_draining_old_instance_is_not_a_finished_turnover` pins it there.
    /// The identical requirement in the post-dwell check had no test with more
    /// than one instance in it, so round 9 of the founder's review changed
    /// that `.all` to `.any` and both existing dwell tests stayed green: over
    /// a one-element iterator the two are the same function.
    ///
    /// A scaled sheep is why it matters. One replica of two coming up, dying
    /// and being restarted under a new pid inside the dwell is a release that
    /// does not work, and `.any` calls it verified because its sibling is
    /// fine.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_one_replica_of_two_restarting_during_the_dwell() {
        let daemon = Listings::new(vec![
            // Turned over: both replicas are new, so `settled` is {200, 201}.
            vec![
                instance(1, ProcStatus::Starting, 200),
                instance(2, ProcStatus::Starting, 201),
            ],
            // After the dwell 201 has died and come back as 202. The first
            // replica still holds, which is exactly what `.any` would accept.
            vec![
                instance(1, ProcStatus::Starting, 200),
                instance(2, ProcStatus::Starting, 202),
            ],
        ]);
        let before = serving_all(&[100, 101]);

        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &before,
            Duration::from_secs(120),
            DWELL,
        )
        .await
        .unwrap();

        assert!(
            !ok,
            "every instance must still be the one that came up, not merely one of them"
        );
    }

    /// fails if the dwell stops noticing a process that is gone entirely by
    /// the end of it - the other half of
    /// `alive_rejects_a_process_that_dies_during_the_dwell`, where shep has
    /// not restarted it yet and there is nothing left to describe.
    #[tokio::test(start_paused = true)]
    async fn alive_rejects_a_flock_that_empties_during_the_dwell() {
        let daemon = Listings::new(vec![
            vec![instance(2, ProcStatus::Starting, 13002)],
            Vec::new(),
        ]);
        let ok = wait(
            &daemon,
            "web",
            Verify::Alive,
            &serving(12835),
            Duration::from_secs(120),
            DWELL,
        )
        .await
        .unwrap();
        assert!(!ok);
    }

    /// fails if `Alive` gives a replacement a fixed twenty seconds to reach
    /// `Online` whatever the app declared.
    ///
    /// A probeless app is marked `Online` by shep's heuristic path when its
    /// `listen_timeout` elapses, and the budget is built from that field.
    /// The post-dwell deadline was a second `DWELL` instead, so any app with
    /// `listen_timeout` past twenty seconds was rolled back healthy. Here
    /// the fake reaches `Online` at the thirty-first poll of a sixty-second
    /// budget, which the old deadline never saw.
    #[tokio::test(start_paused = true)]
    async fn alive_waits_for_online_as_long_as_the_budget_allows() {
        // The turnover lands at once; then `Starting` for well past two
        // dwells worth of polls; then `Online`.
        let mut sequence = vec![vec![instance(2, ProcStatus::Starting, 13002)]; 2];
        // The dwell's own polls: 10s of DWELL at 100ms is 100 polls, and the
        // old cap was another 100. Stay `Starting` for 250 polls.
        sequence.extend(std::iter::repeat_n(
            vec![instance(2, ProcStatus::Starting, 13002)],
            250,
        ));
        sequence.push(vec![instance(2, ProcStatus::Online, 13002)]);
        let daemon = Listings::new(sequence);

        assert!(
            wait(
                &daemon,
                "web",
                Verify::Alive,
                &serving(12835),
                Duration::from_secs(60),
                DWELL
            )
            .await
            .unwrap(),
            "a replacement that reaches Online inside the budget is healthy"
        );
    }
}
