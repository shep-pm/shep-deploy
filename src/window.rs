//! How long [`crate::deploy`] gives a reload before it gives up on it, and
//! how long it then watches the result.
//!
//! Both are sized from the release's own `listen_timeout` and
//! `graceful_timeout`, against shep's reload deadline and its post-drain
//! probe. They live apart from the deploy state machine so that the
//! arithmetic and its reasons sit with the tests that pin it.

use std::time::Duration;

use shep_client::shep_core::config::AppConfig;

use crate::verify;

/// The slack shep adds to its own reload deadline, per instance.
///
/// Copied from `RELOAD_DEADLINE_SLACK` in shep's `supervisor.rs`, where it
/// is a five-second constant covering "scheduling jitter and nothing else"
/// between the two timeouts below. Named here rather than folded into
/// [`budget`]'s arithmetic so a reader can put the two files side by side.
///
/// `crate::optin`'s cutover budget reads this one rather than copying shep's
/// constant into a second file.
pub(crate) const RELOAD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// How long a reload of `instances` instances of this app gets before
/// verification gives up on it.
///
/// This is shep's own formula, not an estimate of it. `Actor::arm_reload_
/// deadline` (`supervisor.rs:3581`) bounds each swap at
/// `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` and ends the
/// swap when that expires, and `advance_reload` walks the instance queue
/// with `pop_front`, one swap at a time, so a whole reload is that per
/// instance.
///
/// # What is measured and what is still read from a file
///
/// `instances` is measured. It comes from [`crate::verify::Generation::instances`], the
/// live pid count, and NOT from `AppConfig::instances`, which is the count
/// the release's Flockfile asks for. Those differ the moment anyone runs
/// `shep stock <sheep> <n>`, a first-class verb that changes what is running
/// without touching any file this crate reads - and the failure that
/// produced is the one this whole window exists to stop: a file saying one
/// instance against two really running gave a budget of 14s for a reload
/// that took about 17, and a healthy release was rolled back.
///
/// A probed `reuse_port` app is the one exception to "shep's own formula":
/// each swap of it probes a second time once the drainee is reaped
/// ([`probes_again_after_drain`]), with `listen_timeout` as that probe's
/// deadline, and the reload deadline does not include it. Without the extra
/// `listen_timeout` per instance the post-drain check could outlast the
/// budget and roll back a healthy release.
///
/// The two timeouts are still read from the release's Flockfile, and that
/// IS an inference, for the same reason
/// `deploy::refuse_ungated_verification` documents about its own read: nothing
/// re-registers a sheep, and `describe` reports status, pid and uptime but
/// never config, so there is no live source to read them from. The exposure
/// is narrower than the instance count's, because changing either one means
/// editing the Flockfile and redeploying, which is the path that brings the
/// new value here anyway - where `shep stock` is a single command that
/// bypasses it.
///
/// # Why it is copied rather than inferred
///
/// Three rounds of review answered "how long may a healthy reload take" by
/// reasoning about shep's timing from outside, and each answer was wrong
/// somewhere new. The first was fixed at ten seconds. The second derived
/// `listen_timeout * instances` and doubled it, which supplies exactly
/// `listen_timeout` of headroom per instance and so holds only while the
/// drain is shorter than the readiness wait - and shep's own defaults are
/// the other way round, `listen_timeout` three seconds against
/// `graceful_timeout` eight. The drain is not incidental to this crate
/// either: [`crate::verify`] requires every OLD instance to be gone, so the
/// whole of it has to fit inside the window.
///
/// There is no floor. A budget shorter than shep's deadline rolls back
/// healthy releases, and one longer buys nothing, because when that deadline
/// expires shep ends the swap itself - there is nothing left to wait for.
/// The five-second slack is already the margin, and it is shep's number for
/// the same jitter.
///
/// `graceful_timeout` is the drain window and `listen_timeout` bounds
/// readiness for EVERY source: `await_ready` (`probes/ready.rs`) wraps
/// `Channel`, `Probe` and `Heuristic` alike in `tokio::time::timeout` and
/// aborts the swap when it elapses. So a probed release's readiness is not
/// unbounded, as an earlier version of this comment claimed - it is bounded
/// by the same field, and an app that needs a minute to compile its client
/// says so by setting `listen_timeout`, which this reads.
pub(crate) fn budget(app: &AppConfig, instances: u32) -> Duration {
    // Saturating throughout, and then held under `verify::MAX_WAIT`. Both
    // timeouts come out of the release's own Flockfile, which the deployed
    // repository writes, and `UpDuration` accepts any value up to `u64::MAX`
    // milliseconds: a plain `+` here panicked, and a sum that did not would
    // have had this dog watching one reload for longer than the host will
    // exist, holding the tree's lock and the poll loop with it.
    let mut per_instance = app
        .listen_timeout
        .as_duration()
        .saturating_add(app.graceful_timeout.as_duration())
        .saturating_add(RELOAD_DEADLINE_SLACK);

    // The post-drain probe is one more `listen_timeout` on top, per swap.
    if probes_again_after_drain(app) {
        per_instance = per_instance.saturating_add(app.listen_timeout.as_duration());
    }

    // A flock with nothing running still gets one instance's worth. Such a
    // reload replaces nothing and can never turn over, so what this buys is
    // a clean failure at a sensible moment rather than an instant one.
    verify::bounded(per_instance.saturating_mul(instances.max(1)))
}

/// Whether shep re-runs the readiness probe after a swap's old instance has
/// drained: a probed app that sets `reuse_port`. The second probe gets the
/// app's `listen_timeout` as its own deadline, after the drain, so it is
/// outside the reload deadline [`budget`] copies from shep and has to be
/// added to it, and it is what [`dwell_for`] watches for a demotion.
fn probes_again_after_drain(app: &AppConfig) -> bool {
    app.reuse_port && app.readiness_probe.is_some()
}

/// How long a verified flock is watched after its turnover.
///
/// [`verify::DWELL`] for most apps. Longer for a probed app that sets
/// `reuse_port`: shep reloads such an app with the two instances overlapping
/// and re-runs the readiness probe AFTER the old instance has drained, with
/// the app's own `listen_timeout` as the deadline, because the old instance
/// may have been the one that answered the first probe. A replacement that
/// fails that second probe is demoted to `Starting` and left there, and that
/// can land anywhere inside the deadline, so a ten-second dwell would record
/// a release deployed and have shep demote it a minute later. Read out of
/// shep-daemon's `finish_swap` and `spawn_verify_task` on 2026-09-04.
pub(crate) fn dwell_for(app: &AppConfig) -> Duration {
    if probes_again_after_drain(app) {
        verify::bounded(
            app.listen_timeout
                .as_duration()
                .saturating_add(RELOAD_DEADLINE_SLACK),
        )
        .max(verify::DWELL)
    } else {
        verify::DWELL
    }
}

#[cfg(test)]
mod tests {
    use shep_client::shep_core::values::UpDuration;

    use super::*;

    /// fails if this crate's reload window stops matching the shepherd's
    /// own reload deadline. That deadline is
    /// `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK` per
    /// instance (`Actor::arm_reload_deadline`, `supervisor.rs:3581`, with
    /// the five-second slack at `supervisor.rs:103`), and instances are
    /// swapped strictly one at a time.
    ///
    /// Be clear about what this test can and cannot do. It restates the
    /// formula, so it catches a change to OURS. It cannot see shep's, so it
    /// cannot catch a change to THEIRS - the numbers below are copied by
    /// hand from a file this crate does not depend on. What covers that
    /// direction is `a_reload_that_uses_its_whole_drain_window_still_deploys`
    /// in the integration tier, which puts a real shepherd through a reload
    /// that spends its drain window and fails if the budget no longer holds
    /// it.
    ///
    /// The arithmetic is spelled out rather than folded, because a test that
    /// recomputes the implementation's expression only pins that it was
    /// typed twice.
    #[test]
    fn the_window_matches_the_shepherds_own_reload_deadline() {
        let mut app: AppConfig =
            toml::from_str("name = 'web'\nscript = './run.sh'").expect("parses");

        // shep's shipping defaults: listen 3s, graceful 8s. 3 + 8 + 5 = 16
        // per instance.
        assert_eq!(app.listen_timeout.as_duration(), Duration::from_secs(3));
        assert_eq!(app.graceful_timeout.as_duration(), Duration::from_secs(8));
        assert_eq!(budget(&app, 1), Duration::from_secs(16));

        // The term round four's derivation dropped. Doubling
        // `listen_timeout` supplied 3s of headroom against an 8s drain, and
        // `verify` needs the whole drain to fit because it waits for every
        // old instance to be gone.
        assert_eq!(budget(&app, 2), Duration::from_secs(32));

        // The instance count is the caller's, measured off the running
        // flock. `AppConfig::instances` is what the file asks for, and
        // `shep stock` moves the two apart - see `budget`'s own doc.
        app.instances = 1;
        assert_eq!(budget(&app, 4), Duration::from_secs(64));

        // And it follows both timeouts, not just the one.
        app.graceful_timeout = UpDuration::from_millis(20_000);
        app.listen_timeout = UpDuration::from_millis(1_000);
        assert_eq!(budget(&app, 3), Duration::from_secs(78));

        // A flock with nothing running gets one instance's worth, so a
        // reload that can replace nothing fails at a sensible moment
        // instead of instantly.
        assert_eq!(budget(&app, 0), budget(&app, 1));
    }

    /// fails if the window stops covering the post-drain probe of a probed
    /// `reuse_port` app. Each swap of such an app probes a second time once
    /// the drainee is reaped, with `listen_timeout` as the deadline, and that
    /// probe comes after the drain rather than inside the readiness wait, so
    /// the swap costs one more `listen_timeout` than the plain formula. Short
    /// of it a healthy release is rolled back; every other app keeps the
    /// plain window.
    #[test]
    fn the_window_covers_a_reuse_port_apps_post_drain_probe() {
        let mut app: AppConfig = toml::from_str(
            "name = 'web'\nscript = './run.sh'\n\
             [readiness_probe]\nkind = 'http'\ntarget = 'http://127.0.0.1:1/'",
        )
        .expect("parses");
        assert_eq!(budget(&app, 1), Duration::from_secs(16), "no reuse_port");

        // Defaults: listen 3s, graceful 8s. 3 + 8 + 5 + 3 = 19 per instance.
        app.reuse_port = true;
        assert_eq!(budget(&app, 1), Duration::from_secs(19));
        assert_eq!(budget(&app, 2), Duration::from_secs(38));

        // It follows `listen_timeout`, not a constant.
        app.listen_timeout = UpDuration::from_millis(10_000);
        assert_eq!(budget(&app, 1), Duration::from_secs(33));

        // Still held under what `verify` will wait at all.
        app.listen_timeout = UpDuration::from_millis(u64::MAX);
        assert_eq!(budget(&app, 3), verify::bounded(Duration::MAX));

        // A `reuse_port` app with no probe is not re-probed.
        app.listen_timeout = UpDuration::from_millis(3_000);
        app.readiness_probe = None;
        assert_eq!(budget(&app, 1), Duration::from_secs(16));
    }

    /// fails if the dwell stops following the app. A probed `reuse_port`
    /// app is re-probed by shep after the old instance drains, with its own
    /// `listen_timeout` as the deadline, and a demotion can land anywhere
    /// inside it; every other app gets the plain dwell.
    #[test]
    fn the_dwell_is_sized_to_the_apps_post_drain_probe() {
        let mut app: AppConfig = toml::from_str(
            "name = 'web'\nscript = './run.sh'\nlisten_timeout = '60s'\n\
             [readiness_probe]\nkind = 'http'\ntarget = 'http://127.0.0.1:1/'",
        )
        .expect("parses");
        assert_eq!(dwell_for(&app), verify::DWELL, "no reuse_port: plain dwell");

        app.reuse_port = true;
        assert_eq!(
            dwell_for(&app),
            Duration::from_secs(65),
            "listen_timeout plus the shepherd's own slack"
        );

        // Never shorter than the plain dwell, however quick the probe.
        app.listen_timeout = UpDuration::from_millis(1_000);
        assert_eq!(dwell_for(&app), verify::DWELL);

        // And never past what `verify` is willing to wait at all.
        app.listen_timeout = UpDuration::from_millis(u64::MAX);
        assert_eq!(dwell_for(&app), verify::bounded(Duration::MAX));

        // A `reuse_port` app with no probe is not re-probed.
        app.readiness_probe = None;
        assert_eq!(dwell_for(&app), verify::DWELL);
    }
}
