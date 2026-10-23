//! A deploy against a running sheep: the swap, a failed build, a rollback.

use std::fs;

use crate::support::fixtures::*;
use crate::support::shepherd::*;

/// `src/main.rs`'s `ROLLED_BACK`, restated because a test binary cannot see
/// a binary crate's internals. A change to one without the other fails this
/// test, which is the point.
const ROLLED_BACK_EXIT: u8 = 12;

/// fails if a deploy of a fixture repo does not reach the running sheep at
/// all. This is the property no unit test can prove: every module below
/// `main` is exercised against a fake `Daemon`, and a fake can never be
/// wrong about whether `reload` actually replaces the process shep is
/// supervising.
#[test]
fn a_real_deploy_swaps_reloads_and_verifies() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first);

    register_web(&shepherd, Readiness::Probe, "probed", "");
    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run at least once", || {
        fs::read_to_string(&out_log)
            .unwrap_or_default()
            .contains("v1")
    });

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", "v2"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    assert!(
        output.status.success(),
        "deploy failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&second),
        "the deploy should report the new sha: {stdout}"
    );

    // `current` really moved to the new release...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&second)
    );

    // ...deploy.toml records it...
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&second), "{state}");

    // ...and the shepherd is actually running the new release's code, not
    // just pointing at it on disk.
    wait_until("v2 to have run", || {
        fs::read_to_string(&out_log)
            .unwrap_or_default()
            .contains("v2")
    });
}

/// fails if a failing build ever reaches the running app. Steps one through
/// five must never touch `current` - this is the property the whole design
/// rests on, and no unit test built out of a fake daemon can prove it,
/// because a fake `reload` can never accidentally run against a real
/// process.
#[test]
fn a_failing_build_leaves_the_previous_release_serving() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first);

    register_web_with_settings(
        &shepherd,
        Readiness::Probe,
        "probed",
        "",
        "command = 'exit 3'\n",
    );
    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let live_pid = described_pid(&shepherd, "web").expect("a pid");

    write_run_script(origin.path(), "v2");
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", "broken"]);

    let output = shepherd.deploy("web");
    assert!(
        !output.status.success(),
        "a failing build must not be reported as a successful deploy"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("build exited with status 3"),
        "the failure should name the build's own exit status: {stderr}"
    );

    // `current` never moved...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&first)
    );

    // ...deploy.toml still names the first release...
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(state.contains(&first), "{state}");

    // ...and the shepherd never reloaded: the running instance is the exact
    // same process, not a fresh one spawned onto the same, unmoved release.
    let pid_after = described_pid(&shepherd, "web").expect("still running");
    assert_eq!(
        live_pid, pid_after,
        "a failed build must never trigger a reload of the running sheep"
    );
}

/// fails if a release that cannot come up is left serving. This is the
/// property the whole crate exists for, and it is the one no mock can
/// prove: `Request::Reload` is an ACCEPTANCE, not a completion, and when
/// shep's own `AwaitReady` fails it keeps the old instance serving. A fake
/// daemon answers `describe` however the test wants and so can never
/// reproduce either fact. Verification that reads status alone passes here
/// on its first poll, reports the broken release deployed, and leaves
/// `current` pointing at it.
///
/// The release breaks by deleting `ready-marker`, which is what the
/// registered readiness probe looks for through the `current` symlink - see
/// [`register_web`]. Nothing about the app changes; the file its probe
/// tests for stops being there.
///
/// `verify = "alive"` rather than the default `probed`, and NOT for speed:
/// the two modes get the same budget now that both floors are gone, and
/// `alive` is the slower of the two by its ten-second dwell. What it buys
/// is coverage. `probed` is what every other test at this tier runs, so
/// this is the only place a real shepherd exercises the `alive` path -
/// including the dwell, which no other test reaches at all.
#[test]
fn a_release_that_cannot_come_up_is_rolled_back_and_the_old_release_serves() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");
    let first = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &first);
    register_web(&shepherd, Readiness::Probe, "alive", "");

    wait_until("the first release to come online", || {
        described_pid(&shepherd, "web").is_some()
    });
    let out_log = shepherd.home().join("logs/web-0-out.log");
    wait_until("v1 to have run", || {
        last_line(&out_log).as_deref() == Some("v1")
    });

    // A release that can never become ready: the probe looks through
    // `current` for a file this commit removes.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["rm", "-q", "ready-marker"]);
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "v2, never ready"]);
    let second = head_of(origin.path());

    let output = shepherd.deploy("web");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("rolled back"),
        "a release that never came up must be reported as rolled back: {stdout}{stderr}"
    );
    assert!(
        !stdout.contains(&format!("deployed {second}")),
        "the broken release must never be reported deployed: {stdout}"
    );
    // The rollback reached the operator's shell, not just the operator's
    // eyes. A unit test proves the mapping; only a real deploy proves the
    // mapping is on the path a rollback actually takes, and this one goes
    // through `Ok(Outcome::RolledBack)`, which exited 0 until today.
    assert_eq!(
        output.status.code(),
        Some(i32::from(ROLLED_BACK_EXIT)),
        "a rolled-back deploy must not report success: {stdout}{stderr}"
    );

    // `current` points at the old release again...
    let current = fs::read_link(shepherd.home().join("deploy/web/current")).expect("current");
    assert_eq!(
        current,
        shepherd.home().join("deploy/web/releases").join(&first),
        "current must be back on the release that works"
    );

    // ...deploy.toml still names it as what is deployed, never the release
    // that failed. The failed sha IS in the file, as the sha the poll loop
    // holds on until the branch moves, so this asks about the key rather
    // than about the file.
    let state = fs::read_to_string(shepherd.home().join("deploy/web/deploy.toml")).expect("state");
    assert!(
        state.contains(&format!("deployed = \"{first}\"")),
        "{state}"
    );
    assert!(
        !state.contains(&format!("deployed = \"{second}\"")),
        "{state}"
    );
    assert!(state.contains(&format!("failed = \"{second}\"")), "{state}");

    // ...and the process the shepherd is supervising is running the old
    // release's code. This is the assertion that fails before generation
    // aware verification: the deploy reported success, `current` stayed on
    // the broken release, and the last thing to run was v2.
    wait_until("the old release to be serving again", || {
        last_line(&out_log).as_deref() == Some("v1")
    });
    assert!(
        described_pid(&shepherd, "web").is_some(),
        "the sheep must still be running after a rollback"
    );
}
