//! Reloads that take longer than a deploy might assume.

use std::fs;
use std::time::{Duration, Instant};

use crate::support::fixtures::*;
use crate::support::shepherd::*;

/// The `listen_timeout` the slow probeless fixture app declares, in
/// seconds.
///
/// Longer than the ten-second window `Verify::Alive` used to be fixed at, on
/// purpose: shorter and the reload fits inside the old window, and the test
/// below cannot fail against the code it exists to pin.
const SLOW_LISTEN: u64 = 12;

/// The app config for the drain-window test: two instances, a short
/// readiness wait, and shep's default drain window stated outright so the
/// arithmetic in the test is readable next to it.
///
/// Two instances because shep swaps them strictly one at a time, so the
/// whole reload costs twice one swap - and [`crate::verify`]'s turnover
/// needs every old instance gone, drain included.
const DRAINING_APP: &str = "instances = 2\ngraceful_timeout = \"8s\"\n";

/// The `listen_timeout` that app declares. Short, so the test spends its
/// time in the drain rather than in readiness.
const DRAINING_LISTEN: u64 = 1;

/// fails if a reload that takes longer than the verification window is
/// treated as a release that failed. This is the case that made `Alive`
/// destructive rather than merely wrong: an app with no readiness gate takes
/// shep's heuristic path, which sleeps its whole `listen_timeout` per
/// instance, so a reload legitimately outlives any window fixed in advance.
///
/// What happened with the window fixed at ten seconds and `listen_timeout`
/// at twelve, measured, deploying a release that was fine:
///
/// ```text
/// shep-deploy: rolling back after it did not come up within 10s ... failed:
/// the daemon reported Internal: web is already being reloaded
/// current   -> releases/<v1>      deployed = "<v1>"      log -> v1 v2
/// ```
///
/// Three failures in sequence: verification gave up on a healthy reload, the
/// rollback's own reload was refused because the first was still running,
/// and what was left was the split state the rollback exists to prevent.
/// Nothing repaired it, and the next poll tried again.
///
/// No mock can reproduce this. It needs a real shepherd's readiness
/// heuristic, a real reload that takes real time, and a real refusal of the
/// second reload.
#[test]
fn a_reload_slower_than_the_old_window_still_deploys() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app("v1", Readiness::Heuristic(SLOW_LISTEN), "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first);
    register_web(&shepherd, Readiness::Heuristic(SLOW_LISTEN), "alive", "");

    wait_until("the first release to come online", || {
        described_online(&shepherd, "web")
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "v2"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a good release must not fail its own deploy: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(&format!("deployed {second}")),
        "the new release must be reported deployed: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains("rolled back") && !stderr.contains("split"),
        "nothing here should roll back: {stdout}{stderr}"
    );

    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    // And the process really is on the new release, which is the half of
    // "deployed" that a symlink cannot prove.
    assert_eq!(
        last_line(&out_log).as_deref(),
        Some("v2"),
        "the running process must be executing the new release"
    );
}

/// fails if the verification window stops covering a reload that spends its
/// whole drain window. This is the term three rounds of derivation left out:
/// shep bounds one swap at `listen_timeout + graceful_timeout +
/// RELOAD_DEADLINE_SLACK` (`arm_reload_deadline`, `supervisor.rs:3581`) and
/// swaps instances one at a time, and `crate::verify` needs every OLD
/// instance gone before it calls the deploy verified - so the drain is
/// inside the window this crate has to wait out, not outside it.
///
/// The shipping defaults are the wrong way round for a derivation built on
/// `listen_timeout` alone: three seconds of readiness against eight of
/// drain. This app makes that gap wider still and then refuses to die
/// politely, so a reload takes about eighteen seconds where a window of
/// `listen_timeout x instances x 2` would have been four.
///
/// This is also the only test at any tier that runs a multi-instance
/// reload.
#[test]
fn a_reload_that_uses_its_whole_drain_window_still_deploys() {
    let shepherd = Shepherd::new();
    let readiness = Readiness::Heuristic(DRAINING_LISTEN);
    let origin = origin_with_app("v1", readiness, DRAINING_APP);
    write_stubborn_run_script(origin.path(), "v1");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "stubborn v1"]);

    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first);
    register_web(&shepherd, readiness, "alive", DRAINING_APP);

    wait_until("both instances to come online", || {
        described_online(&shepherd, "web")
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    write_stubborn_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "stubborn v2"]);
    let second = head_of(origin.path());

    let started = Instant::now();
    let output = shepherd.deploy("web");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a healthy reload that used its drain window must not fail its own \
         deploy: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(&format!("deployed {second}")),
        "the new release must be reported deployed: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains("rolled back") && !stderr.contains("split"),
        "nothing here should roll back: {stdout}{stderr}"
    );

    // The fixture must actually have spent the drain, or this test would
    // pass for the wrong reason the day the trap stops working. Two
    // instances at eight seconds each, minus room for the machine.
    assert!(
        elapsed >= Duration::from_secs(14),
        "the app cannot have used its drain window in {elapsed:?} - this test \
         is no longer testing the term it exists for"
    );

    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    assert_eq!(
        last_line(&out_log).as_deref(),
        Some("v2"),
        "the running process must be executing the new release"
    );
}
