//! Putting a half-done cutover back.

use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::ProcessInfo;
use tokio::time::{Instant, sleep};

use super::attempt::cutover_budget;
use crate::daemon::Daemon;
use crate::verify::POLL;

/// Removes every instance this cutover added, and puts the shepherd's
/// persisted record back the way it was.
///
/// Two separate repairs, because the `Start` did two separate things.
///
/// Deleting the newcomer undoes the process. It does NOT undo the record:
/// `FlockRegistry` is keyed by name and records on every accepted `Start`,
/// and the surviving original keeps that name alive so the roll's own prune
/// never drops the poisoned entry. Left alone, `shep muster` after a reboot
/// brings the sheep back from the release that was just rejected, and this
/// dog's own [`crate::roll::registered`] reads the wrong `cwd` from then on.
///
/// So the record is put back the only way the wire allows: a second `Start`
/// carrying the ORIGINAL config, which re-records it, followed by deleting
/// the instance that `Start` necessarily spawned. There is no request that
/// registers without spawning; `RegisterAtRest` is a supervisor command
/// muster uses and is not on the wire. The cost is a second instance of the
/// app alive for as long as those two calls take.
///
/// Answers both halves separately, because the error text needs both and
/// they fail independently: a newcomer left registered is visible in
/// `shep flock`, and a roll left poisoned is not.
pub(super) async fn undo_start<D: Daemon>(
    daemon: &D,
    sheep: &str,
    previous: &[u32],
    original: AppConfig,
) -> Undone {
    let Ok(flock) = daemon.describe(sheep).await else {
        // Nothing is known about what needs removing, so nothing is
        // claimed. The error text sends the operator to `shep describe`.
        return Undone {
            removed: false,
            recorded: false,
        };
    };
    // Individual failures are recorded rather than returned: this path is
    // already failing, and an operator needs the reason they got here
    // rather than a second error about the cleanup.
    let mut removed = drain(daemon, &flock, previous).await;

    let patience = cutover_budget(&original);
    // The rows the repair registered, by the shepherd's own account. Waiting
    // for "a row that is new" instead let a cutover newcomer that spawned
    // late stand in for the repair instance, and the wait ended with the
    // repair's own rows still to come and then left registered.
    let Ok(repair) = daemon.start(vec![original]).await else {
        return Undone {
            removed,
            recorded: false,
        };
    };

    // The repair `Start` spawned an instance, and a `Start` is an acceptance:
    // the shepherd spawns afterwards. Waited for, the way phase one of
    // `attempt` waits, because describing at once found nothing, reported
    // everything as removed, and left a second instance of the app coming up
    // on the original config to fight the survivor for its port.
    let deadline = Instant::now() + patience;
    loop {
        let Ok(flock) = daemon.describe(sheep).await else {
            // The roll IS re-recorded: the start was accepted. What is not
            // known is whether the instance it spawned is gone, and it is
            // reported as not, which is the answer that sends the operator
            // to look.
            return Undone {
                removed: false,
                recorded: true,
            };
        };
        // Every row the repair registered, present. Then everything that is
        // not an original goes: the repair's rows, and any cutover newcomer
        // that spawned after the first drain looked.
        let all_present = repair
            .iter()
            .all(|id| flock.iter().any(|info| info.id == *id));
        if all_present {
            removed &= drain(daemon, &flock, previous).await;
            break;
        }
        if Instant::now() >= deadline {
            // Whatever did arrive is taken down; what did not is reported.
            removed = false;
            let _ = drain(daemon, &flock, previous).await;
            break;
        }
        sleep(POLL).await;
    }

    Undone {
        removed,
        recorded: true,
    }
}

/// What [`undo_start`] managed to put back.
///
/// Two bools rather than one, because the `Start` did two things and either
/// repair can fail on its own. Folding them together would make the error
/// text claim one when only the other held.
pub(super) struct Undone {
    /// Whether every instance this cutover added was removed.
    pub(super) removed: bool,
    /// Whether the shepherd's persisted roll was re-recorded.
    pub(super) recorded: bool,
}

/// Deletes every row in `flock` whose id is not in `previous`, answering
/// whether all of them went.
///
/// "Not in `previous`" is "not seen before the cutover's `Start`", which is
/// a wider net than "added by this cutover": an instance an operator started
/// by hand inside the window goes with it. That window is a few seconds
/// under a lock the operator's own commands respect, so the wider net is
/// accepted rather than narrowed by guessing which row was whose.
async fn drain<D: Daemon>(daemon: &D, flock: &[ProcessInfo], previous: &[u32]) -> bool {
    let mut all = true;
    for info in flock.iter().filter(|info| !previous.contains(&info.id)) {
        if daemon.delete(info.id).await.is_err() {
            all = false;
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::error::Error;
    use crate::optin::cut_over;
    use crate::optin::test_support::{
        NEWCOMER_ID, REPAIR_ID, cutover_fixture, cutover_fixture_dies_and_refuses_deletes,
        cutover_fixture_dies_and_refuses_repair,
        cutover_fixture_dies_and_refuses_the_second_delete, cutover_fixture_dies_during_dwell,
        cutover_fixture_never_appears,
    };
    /// fails if an abandoned cutover leaves shep's persisted roll naming
    /// the release that was just rejected. `FlockRegistry` is name-keyed
    /// and records on every accepted Start, and deleting the newcomer does
    /// NOT undo it, because the surviving original keeps the name alive so
    /// the roll's prune never drops it. Verified end to end: after the
    /// delete the roll named the new release while the live pid executed in
    /// the old one, and `shep kill` plus a restart plus `shep muster`
    /// brought the sheep back from the abandoned release.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_puts_the_original_config_back_in_the_roll() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;
        let original = prepared.previous_config.clone();

        cut_over(&daemon, prepared).await.expect_err("gives up");

        let last = daemon.started().last().cloned().expect("a repair Start");
        assert_eq!(
            last.cwd, original.cwd,
            "the roll is re-recorded at the old cwd"
        );
        assert_eq!(
            daemon.deleted().len(),
            2,
            "the newcomer, and the instance the repair Start spawned"
        );
    }

    /// fails if a scaled original's repair leaves an instance behind. The
    /// repair `Start` spawns one row per instance the original asks for,
    /// and they need not appear on the same poll; waiting for the first and
    /// draining that listing left the rest registered while `removed` said
    /// true, and every later deploy respawned them on the old config.
    #[tokio::test(start_paused = true)]
    async fn a_scaled_repair_is_drained_whole_however_its_rows_arrive() {
        let (daemon, mut prepared, _dirs) = cutover_fixture_dies_during_dwell().await;
        let daemon = daemon.staggering_repairs_by(POLL * 4);
        prepared.previous_config.instances = 3;

        cut_over(&daemon, prepared).await.expect_err("gives up");

        for offset in 0..3 {
            assert!(
                daemon.is_deleted(REPAIR_ID + offset),
                "repair row {offset} left registered: {:?}",
                daemon.deleted()
            );
        }
    }

    /// fails if a cutover newcomer that spawns late is taken for the repair
    /// instance. The cutover asked for two, the shepherd spawned one, the
    /// cutover gave up waiting, and the second appears after the first
    /// drain and before the repair's own row. Counting "a new row" ended
    /// the wait there and left the repair row registered; the repair's ids
    /// are the shepherd's own answer, and both rows have to go.
    #[tokio::test(start_paused = true)]
    async fn a_late_cutover_newcomer_is_not_taken_for_the_repair_instance() {
        let (daemon, mut prepared, _dirs) = cutover_fixture().await;
        prepared.app.instances = 2;
        let late = cutover_budget(&prepared.app) + Duration::from_millis(100);
        let daemon = daemon.spawning_at_most(1).with_a_late_newcomer_after(late);

        cut_over(&daemon, prepared)
            .await
            .expect_err("gives up on the missing newcomer");

        assert!(daemon.is_deleted(REPAIR_ID), "{:?}", daemon.deleted());
        assert!(
            daemon.is_deleted(NEWCOMER_ID - 1),
            "the late newcomer goes too: {:?}",
            daemon.deleted()
        );
    }

    /// fails if a repair that could not be made is glossed over.
    /// [`Error::CutOver`] used to say the original was serving
    /// "unchanged", which is true of the process and false of the persisted
    /// flock: a reboot would silently resurrect the rejected release. When
    /// the repair fails the operator has to be told that specifically,
    /// because it is the half they cannot see in `shep flock`.
    #[tokio::test(start_paused = true)]
    async fn a_failed_roll_repair_is_named_not_glossed() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_repair().await;
        let err = cut_over(&daemon, prepared).await.expect_err("gives up");
        let shown = err.to_string();
        assert!(shown.contains("muster"), "{shown}");
        assert!(
            shown.contains("reboot") || shown.contains("restart"),
            "{shown}"
        );
    }

    /// fails if a cutover that never started anything says it removed
    /// something. With this fixture the shepherd accepts the `Start` and
    /// produces no row at all, so the first drain finds nothing and
    /// succeeds vacuously - and the sentence after `why` used to read "The
    /// instance it added has been removed" directly beneath "No new
    /// instance appeared". Two adjacent sentences contradicting each other
    /// is how an operator decides a message is boilerplate and stops
    /// reading it, which matters because the paragraph after them is the
    /// one that keeps them off `shep deploy`.
    #[tokio::test(start_paused = true)]
    async fn a_cutover_that_added_nothing_does_not_claim_to_have_removed_it() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("No new instance appeared"), "{shown}");
        assert!(
            !shown.contains("instance it added has been removed"),
            "it cannot have removed what it never added: {shown}"
        );
        assert!(
            shown.contains("Nothing this cutover started is left registered"),
            "{shown}"
        );
    }

    /// fails if a repair instance that could not be deleted is described as
    /// the one under the deploy tree. `undo_start` drains twice and only
    /// the first drain removes something started from the deploy tree: the
    /// repair `Start` re-registers the ORIGINAL config, so the instance it
    /// spawns has the operator's own checkout as its cwd, identical to the
    /// survivor's. A hint keyed on cwd is then wrong, and wrong in the
    /// direction that has the operator hunting for a row that does not
    /// exist while a duplicate of their app keeps running.
    ///
    /// `removed: false` with `repaired: true` is also a combination no
    /// other fixture produces.
    #[tokio::test(start_paused = true)]
    async fn a_repair_instance_left_behind_is_not_described_by_its_cwd() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_the_second_delete().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            matches!(
                err,
                Error::CutOver {
                    removed: false,
                    repaired: true,
                    ..
                }
            ),
            "{err}"
        );
        let shown = err.to_string();
        assert!(
            !shown.contains("cwd under the deploy tree"),
            "the hint would point at the wrong instance: {shown}"
        );
        assert!(shown.contains("shep delete <id>"), "{shown}");
    }

    /// fails if an abandoned cutover claims to have removed an instance it
    /// could not remove. `undo_start` swallows each failed delete so the
    /// operator gets the reason they are here rather than a second error
    /// about the cleanup, which makes this message the only place a failed
    /// one is ever mentioned. An operator told the newcomer is gone has no
    /// reason to look for it, and what is left behind is a second instance
    /// of their app running the release that was just rejected.
    #[tokio::test(start_paused = true)]
    async fn a_newcomer_that_could_not_be_removed_is_named_not_claimed_gone() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_and_refuses_deletes().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("could NOT be removed"), "{shown}");
        assert!(shown.contains("shep describe bpm"), "{shown}");
    }
}
