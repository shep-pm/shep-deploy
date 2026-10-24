//! The cutover's `Start` and the watching that follows it.

use std::time::Duration;

use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::ProcessInfo;
use tokio::time::{Instant, sleep};

use crate::daemon::Daemon;
use crate::error::Error;
use crate::verify::{self, DWELL, Generation, POLL, is_alive};
use crate::window::RELOAD_DEADLINE_SLACK;

/// What a cutover's `Start` and the watching that follows it came to.
///
/// The variants turn on what has to be cleaned up rather than on what went
/// wrong, which is why [`CutOver::NotStarted`] is its own arm: before the
/// shepherd accepts the `Start` nothing has been spawned and nothing has
/// been recorded.
pub(super) enum CutOver {
    /// A new instance came up and was still the same instance after the
    /// dwell.
    Done,
    /// The shepherd refused the `Start`.
    NotStarted(Error),
    /// The `Start` was accepted and the newcomer did not come up, or did
    /// not stay up.
    NotVerified(String),
    /// The `Start` was accepted and something failed while watching.
    Failed(Error),
}

/// Whether a newcomer's row says it is dead.
///
/// The same answer with or without a pid: a row the shepherd has accepted
/// and not yet spawned has no pid and says `Starting`, which [`is_alive`]
/// accepts, and every other status without a pid is a process on its way
/// out or already gone. One predicate rather than two hand-kept complements
/// of `ProcStatus`, so a status added upstream is judged one way.
fn died(info: &ProcessInfo) -> bool {
    !is_alive(info)
}

/// What a first cutover most often runs into, said once.
///
/// Appended to every `why` where it could be the cause, and deliberately not
/// to the one where it cannot: a `Start` the shepherd accepted that then
/// produced no row at all never got as far as binding anything.
const PORT_COLLISION: &str = "The first cutover is the one deploy that runs two instances at \
     once, so an app that does not bind with SO_REUSEPORT cannot take its own port while the \
     original still holds it. Every deploy after the first replaces the instance rather than \
     joining it, and does not meet this. If the app cannot set SO_REUSEPORT, `shep stop` it, \
     remove the tree named above, and run setup again: with the port free the newcomer binds, \
     and the cutover is the one deploy allowed to be down for a moment anyway.";

/// How long a newcomer gets to APPEAR before the cutover gives up on it.
///
/// Only to appear. The dwell below is what actually decides health, so
/// this does not have to cover a slow readiness path: a probeless app that
/// sits in `Starting` well past eight seconds is fine here, because
/// [`is_alive`] accepts `Starting` and the question at this stage is
/// only whether a process exists at all.
///
/// A whole `listen_timeout` plus the slack shep allows itself, rather than
/// `crate::deploy`'s per-instance product: a cutover's `Start` spawns its
/// instances together and drains nothing, where a reload replaces every
/// instance one at a time and has to wait out each drain.
pub(super) fn cutover_budget(app: &AppConfig) -> Duration {
    // Saturating and capped, for the reason `crate::window`'s `budget` gives:
    // `listen_timeout` is the repository's number, not the operator's.
    verify::bounded(
        app.listen_timeout
            .as_duration()
            .saturating_add(RELOAD_DEADLINE_SLACK),
    )
}

/// Registers `app`, waits for a newcomer, and watches it for a dwell.
///
/// This function cannot return an error, and that is the point rather than
/// tidiness: every fallible step after the shepherd accepts the `Start`
/// happens here, so [`super::cut_over`]'s match is exhaustive over what has to be
/// cleaned up instead of over what went wrong, and a step added here later
/// has nowhere to escape to. Same structure as `crate::deploy::reload::land`,
/// for the same reason.
///
/// # This check is `Verify::Alive`, and it is weaker than the deploy path's
///
/// It establishes that a new process was spawned and that after [`DWELL`]
/// it is the same process, not errored, and has not restarted. It does NOT
/// establish that the release serves anything, and it cannot.
///
/// The reason is shep's, not this crate's. `handle_ready_result` marks a
/// FRESH SPAWN `Online` when `listen_timeout` elapses whatever the readiness
/// probe said (Rin, 2026-08-08: erroring instead would turn a slow start
/// into a restart loop), and defers to `reload_ready_result`, which DOES
/// abort on a readiness timeout, only for a reload replacement. So `Online`
/// on this path is "the probe passed OR the timeout elapsed" with nothing
/// on the wire distinguishing the two, and [`crate::verify`] is sound
/// precisely because it verifies a reload. A check of `is_new && Online`
/// here returns success in milliseconds for a release that is already dead.
///
/// `state.verify` is deliberately not consulted. `Probed` is unavailable on
/// this path, so honouring it would be a lie and refusing a target that
/// lacks a gate it could not use would be incoherent. The weakness is
/// bounded rather than permanent: once this lands the target is an ordinary
/// deploy target and every deploy after it gets full turnover verification
/// and auto-rollback.
///
/// What it does catch is the case this task is arranged around. A newcomer
/// that cannot bind its port because the original still holds it exits, and
/// shep either errors it or respawns it under a new pid, and the dwell sees
/// both. What passes anyway is a release that starts, stays up, and serves
/// nothing, which is what `alive` has always meant.
pub(super) async fn attempt<D: Daemon>(
    daemon: &D,
    sheep: &str,
    app: AppConfig,
    before: &Generation,
    previous: &[u32],
) -> CutOver {
    let patience = cutover_budget(&app);
    // How many the `Start` spawns. A scaled sheep spawns them together, and
    // freezing the generation on the first to appear verified a subset and
    // then deleted every original: the half-strength defect
    // `Generation::has_turned_over` guards against on the deploy path.
    let wanted = usize::try_from(app.instances.max(1)).unwrap_or(1);

    if let Err(source) = daemon.start(vec![app]).await {
        return CutOver::NotStarted(source);
    }
    let started_at = Instant::now();
    let deadline = started_at + patience;

    // Phase one: every newcomer exists and none has already failed.
    //
    // A newcomer is a row whose id the flock did not have before the
    // `Start`. By id rather than by pid, because an instance that died hard
    // has no pid and a pid-only reading made it invisible: the loop then
    // burned the whole budget and reported that nothing appeared, which is
    // the one message that deliberately withholds the port-collision hint,
    // for a port collision. An original that crashed and respawned keeps its
    // id and is not a newcomer, whatever its pid.
    let arrived = loop {
        let flock = match daemon.describe(sheep).await {
            Ok(flock) => flock,
            Err(source) => return CutOver::Failed(source),
        };

        let newcomers: Vec<&ProcessInfo> = flock
            .iter()
            .filter(|info| !previous.contains(&info.id))
            .collect();

        // A row with a pid is judged by `is_alive`. A row without one is
        // judged only by a status that says it died (`Errored`,
        // `WaitingRestart`): a row the shepherd has accepted and not yet
        // spawned has no pid either, and whatever status it carries in that
        // instant is not a verdict.
        if newcomers.iter().any(|info| died(info)) {
            return CutOver::NotVerified(format!(
                "The new instance failed before it finished starting. {PORT_COLLISION}"
            ));
        }
        // Up, with a process: the dwell below compares pids.
        let up: Vec<&ProcessInfo> = newcomers
            .iter()
            .copied()
            .filter(|info| before.is_new(info))
            .collect();
        if up.len() >= wanted {
            break Generation::of_infos(&up);
        }
        if Instant::now() >= deadline {
            return CutOver::NotVerified(if up.is_empty() {
                format!(
                    "No new instance appeared within {}s, although the shepherd accepted the \
                     start.",
                    started_at.elapsed().as_secs()
                )
            } else {
                format!(
                    "Only {} of the {wanted} instances the start asked for appeared within \
                     {}s. {PORT_COLLISION}",
                    up.len(),
                    started_at.elapsed().as_secs()
                )
            });
        }
        sleep(POLL).await;
    };

    // Phase two: the dwell, which is what actually decides this.
    sleep(DWELL).await;
    let flock = match daemon.describe(sheep).await {
        Ok(flock) => flock,
        Err(source) => return CutOver::Failed(source),
    };

    let survivors: Vec<&ProcessInfo> = flock
        .iter()
        .filter(|info| arrived.holds(info) && is_alive(info))
        .collect();

    if survivors.len() != arrived.instances() as usize {
        // A different pid means shep respawned it, which means it died.
        return CutOver::NotVerified(format!(
            "The new instance did not stay up for {}s after starting. {PORT_COLLISION}",
            DWELL.as_secs()
        ));
    }
    // NOT belt and braces, and an earlier comment here saying so was wrong
    // in the direction that matters. `Generation::is_new` compares pids and
    // nothing else, and the generation was captured before the `Start`, so
    // an ORIGINAL that crashed and respawned before the real newcomer
    // appeared reads as a newcomer: phase one adopts it, and the dwell
    // finds that same pid still alive and not errored. The restart count is
    // the only thing left that rejects it. Without this check the cutover
    // returns `Done` having verified nothing, and then deletes the healthy
    // original by id.
    //
    // Not an exotic shape either. The newcomer is fighting the original for
    // its port, which is the premise of this whole task, and the original
    // is the side that can lose. Pinned by
    // `an_original_that_respawns_is_not_mistaken_for_the_newcomer`.
    if survivors.iter().any(|info| info.restarts > 0) {
        return CutOver::NotVerified(format!(
            "The instance this cutover adopted had already restarted {}s later, so what is \
             running is not the process the start spawned. {PORT_COLLISION}",
            DWELL.as_secs()
        ));
    }

    CutOver::Done
}

#[cfg(test)]
mod tests {
    use shep_client::shep_core::status::ProcStatus;

    use super::*;
    use crate::optin::cut_over;
    use crate::optin::test_support::{
        NEWCOMER_ID, comes_up, cutover_fixture_crash_looping, cutover_fixture_dies_during_dwell,
        cutover_fixture_from, cutover_fixture_never_appears, cutover_fixture_online_then_gone,
        cutover_fixture_original_respawns,
    };

    /// A gated sheep that runs two instances.
    const TWO_INSTANCES: &str = ",\"wait_ready\":true,\"instances\":2";

    /// fails if a pidless newcomer is judged by anything but its status.
    /// A row the shepherd has accepted and not yet spawned has no pid and
    /// says `Starting`; one that died hard has no pid and says `Errored`.
    /// The first is waited for and the second ends the cutover.
    #[test]
    fn a_pidless_newcomer_is_dead_only_when_its_status_says_so() {
        let row = |status| ProcessInfo::builder(99, "bpm", status).build();
        for alive in [ProcStatus::Starting, ProcStatus::Online] {
            assert!(
                !died(&row(alive)),
                "{alive:?} without a pid is not yet a verdict"
            );
        }
        for dead in [
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
            ProcStatus::Stopped,
            ProcStatus::Stopping,
        ] {
            assert!(died(&row(dead)), "{dead:?} without a pid is dead");
        }
        let with_pid = ProcessInfo::builder(99, "bpm", ProcStatus::Stopping)
            .pid(Some(4242))
            .build();
        assert!(
            died(&with_pid),
            "with a pid the status decides the same way"
        );
    }

    /// fails if a scaled sheep's cutover settles for fewer newcomers than
    /// the `Start` asked for. With `instances = 2` and a shepherd that
    /// spawned one, the cutover used to freeze its generation on that one,
    /// verify it, and delete both originals: the sheep came back at half
    /// strength with the deploy reported landed.
    #[tokio::test(start_paused = true)]
    async fn a_scaled_cutover_waits_for_every_instance_the_start_asked_for() {
        let (daemon, prepared, _dirs) =
            cutover_fixture_from(TWO_INSTANCES, &[7, 8], comes_up(), None).await;
        let daemon = daemon.spawning_at_most(1);

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("one of two is not a cutover");

        let shown = err.to_string();
        assert!(shown.contains("Only 1 of the 2"), "{shown}");
        assert!(
            !daemon.deleted().contains(&7) && !daemon.deleted().contains(&8),
            "the originals are kept: {:?}",
            daemon.deleted()
        );
    }

    /// fails if a scaled sheep whose newcomers all appear is not cut over.
    /// The other half of the test above: two asked for, two spawned, both
    /// originals replaced.
    #[tokio::test(start_paused = true)]
    async fn a_scaled_cutover_lands_when_every_instance_appears() {
        let (daemon, prepared, _dirs) =
            cutover_fixture_from(TWO_INSTANCES, &[7, 8], comes_up(), None).await;

        cut_over(&daemon, prepared).await.expect("cuts over");

        assert_eq!(daemon.deleted(), vec![7, 8]);
    }

    /// fails if a new instance that came up and then died is left
    /// registered. The
    /// likeliest cause is the one the design names: both instances are
    /// alive at once, so without SO_REUSEPORT the new one cannot bind the
    /// port and dies. Leaving it behind gives the operator a permanently
    /// errored second instance of their app and an old one still serving,
    /// with nothing saying which is which.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_dies_during_the_dwell_is_deleted_and_the_old_one_kept() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            daemon.deleted().contains(&NEWCOMER_ID),
            "the newcomer is removed"
        );
        assert!(!daemon.deleted().contains(&7), "the old instance is kept");
        let shown = err.to_string();
        assert!(shown.contains("SO_REUSEPORT"), "{shown}");
    }

    /// fails if a release that shep called `Online` without the probe ever
    /// passing is accepted. This is the whole of finding F1 and it is the
    /// engine plan's round-3 blocker on a different path: shep marks a
    /// FRESH SPAWN Online when `listen_timeout` elapses whatever the probe
    /// said, and aborts only a RELOAD replacement on a readiness timeout.
    /// So `is_new(info) && Online` establishes nothing about a Start, and
    /// measured against a real shepherd it returned Done in 15.6ms on a
    /// dead-on-arrival release, after which the healthy original is deleted
    /// by id and `state.deployed` written.
    ///
    /// The fixture is the exact shape shep produces: the newcomer reports
    /// Online, on time, and is gone by the dwell.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_went_online_without_serving_is_still_rejected() {
        let (daemon, prepared, _dirs) = cutover_fixture_online_then_gone().await;

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("the dwell catches it");

        // Which failure, not merely that there was one: the fixture would
        // also error if the fake stopped answering, and that would pass a
        // bare `expect_err` while establishing nothing about the dwell.
        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert!(err.to_string().contains("did not stay up"), "{err}");
    }

    /// fails if a newcomer that crash-loops through the dwell is accepted.
    /// An app whose release cannot run is restarted by shep, so it is
    /// present at every poll and present at the dwell, under a DIFFERENT
    /// pid each time. Pid identity across the dwell is what catches it, and
    /// `restarts` moving is the second signal.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_crash_loops_through_the_dwell_is_rejected() {
        let (daemon, prepared, _dirs) = cutover_fixture_crash_looping().await;

        let err = cut_over(&daemon, prepared)
            .await
            .expect_err("the pids moved");

        // The pid check, named: a crash-looped newcomer is present at the
        // same count under a different pid, so this is the branch that has
        // to fire rather than the restart count beside it.
        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert!(err.to_string().contains("did not stay up"), "{err}");
    }

    /// fails if a cutover that never spawned anything blames the port. The
    /// SO_REUSEPORT paragraph is the likeliest cause of a newcomer that
    /// came up and died, and it is impossible for one the shepherd accepted
    /// and never produced a row for: nothing ever got as far as binding.
    /// An operator sent after a port that was never contended is an
    /// operator who stops reading at the first plausible sentence.
    #[tokio::test(start_paused = true)]
    async fn a_cutover_that_spawned_nothing_does_not_blame_the_port() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(!shown.contains("SO_REUSEPORT"), "{shown}");
        assert!(shown.contains("No new instance appeared"), "{shown}");
    }

    /// fails if the ORIGINAL, respawned, is mistaken for the release this
    /// cutover just started. A newcomer used to be any row whose pid the
    /// pre-`Start` generation never had, so an original that crashed and
    /// came back under a new pid looked exactly like one: phase one adopted
    /// it, and the dwell found the same pid still alive. Newcomers are now
    /// told by id, which a respawn keeps, and the restart count on the dwell
    /// is the second wall rather than the only one.
    ///
    /// What is at stake is the whole of this task. Accept the respawned
    /// original and the cutover reports `Done` having verified nothing,
    /// then deletes that healthy original by id, leaving the sheep with no
    /// instance at all. And the shape is the premise of the task rather
    /// than a corner: the newcomer is fighting the original for its port.
    #[tokio::test(start_paused = true)]
    async fn an_original_that_respawns_is_not_mistaken_for_the_newcomer() {
        let (daemon, prepared, _dirs) = cutover_fixture_original_respawns().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        // By id, now: the original keeps its id across the respawn, so it is
        // never a newcomer whatever its pid, and what the cutover reports is
        // that no newcomer appeared at all. The restart check on the dwell
        // stays as a second wall behind this one.
        assert!(
            err.to_string().contains("No new instance appeared"),
            "{err}"
        );
        assert!(
            !daemon.deleted().contains(&7),
            "the healthy original is kept: {:?}",
            daemon.deleted()
        );
    }
}
