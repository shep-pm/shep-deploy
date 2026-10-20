//! Running a prepared command in a process group of its own, bounded by a
//! budget and killed whole.

use std::env;
use std::path::Path;
use std::time::Duration;

use std::os::unix::process::CommandExt as _;

use tokio::process::Command;
use tokio::time::timeout;

use crate::error::Error;

/// Runs `child` to completion inside a fresh process group, abandoning the
/// group if it outlives `budget`, and killing it when the command ends either
/// way.
///
/// `child` arrives fully configured: program, arguments, working directory,
/// environment and any uid/gid drop are the caller's. What this adds is the
/// group and the clock. `release` names the directory for the errors that
/// have a path.
///
/// # Errors
/// [`Error::Io`] naming `release` if the group holder or `child` cannot be
/// spawned, or the wait fails. [`Error::Build`] if `child` exits non-zero or
/// is killed by a signal, naming the exit status when there is one.
/// [`Error::BuildTimedOut`] if it runs past `budget`.
pub(crate) async fn run_in_group(
    mut child: Command,
    release: &Path,
    budget: Duration,
) -> Result<(), Error> {
    // Its own process group, so a build that has to be abandoned can be
    // abandoned whole. `sh -c` is a shell, and what hangs is usually something
    // it started rather than the shell itself, so killing only the shell would
    // leave the real process running while the deploy reports a failure. Same
    // shape and same reason as `crate::shared::run_git_within`.
    //
    // The group's leader is not the build but a holder: a shell that reads
    // one line from a pipe this dog keeps the write end of, so it sits
    // there until the group is killed. A process group's id is its leader's
    // pid, and the system does not reuse a pid while a group with that id
    // exists. The build is reaped before the group is signalled below,
    // and a reaped leader's pid is free to be handed to whatever forks
    // next; with the build as leader, `killpg` after the wait could in
    // principle reach an unrelated process that had taken the pid and
    // started a group of its own, as root. With the holder as leader the
    // id is reserved for as long as the holder lives, and the holder lives
    // until this function kills it.
    let mut holder = std::process::Command::new("sh");
    holder
        .args(["-c", "read _"])
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    if let Some(path) = env::var_os("PATH") {
        holder.env("PATH", path);
    }
    let mut holder = holder.spawn().map_err(Error::at(release))?;
    let group = holder.id();
    let group_id = i32::try_from(group).map_err(|_| Error::Io {
        path: release.to_owned(),
        source: std::io::Error::other("the group holder's pid does not fit a pid_t"),
    })?;
    child.process_group(group_id);

    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(source) => {
            crate::shared::kill_group(group);
            let _ = holder.wait();
            return Err(Error::Io {
                path: release.to_owned(),
                source,
            });
        }
    };

    // Bounded, because an unbounded wait here does not stop one target, it
    // stops the dog. `crate::poll::tick` deploys targets one at a time, so a
    // build that never finishes holds that loop forever: no other target
    // deploys, no smit is refreshed, and nothing is logged, because from the
    // loop's point of view nothing has gone wrong. That is the failure
    // `git_timeout` was added for, one subprocess along.
    //
    // A test that drives a real build cannot use `#[tokio::test(start_paused
    // = true)]` because of this line. Paused time advances to the next
    // deadline whenever the runtime is idle, and a real subprocess makes it
    // idle, so the budget expires at once whatever it is set to. See
    // `crate::deploy`'s `a_current_that_moved_during_the_build_is_not_swapped_over`.
    let status = match timeout(budget, child.wait()).await {
        Ok(waited) => waited,
        Err(_) => {
            abandon(&mut child, group).await;
            let _ = holder.wait();
            return Err(Error::BuildTimedOut { after: budget });
        }
    };

    // The group goes when the build does, however the build ended. `sh -c`
    // exiting is not the same as the build being over: a command can leave a
    // job running behind it, and this module's own comments have said so since
    // round 4, because that job is the thing every containment check below is
    // racing. It can swap a component between a name being approved and the
    // same name being opened, and one was measured winning that race in 0.03s.
    //
    // Killing it here removes the racer instead of out-manoeuvring it, which no
    // amount of re-checking can do on its own. It also stops a build leaving a
    // process behind that outlives the deploy and can still write to a release
    // an operator has been told is live.
    //
    // A process that genuinely means to persist calls `setsid` and leaves the
    // group, so a compiler daemon survives this and a stray `&` does not. That
    // is the right split: one asked to outlive its parent, the other just did.
    //
    // Safe after the wait because the group is the holder's, not the build's:
    // see where it is spawned. The holder goes with the group, and is reaped.
    crate::shared::kill_group(group);
    let _ = holder.wait();
    let status = status.map_err(Error::at(release))?;

    if !status.success() {
        return Err(Error::Build {
            status: status.code(),
        });
    }

    Ok(())
}

/// Kills an abandoned build's whole process group, `group` being the pid of
/// the holder that leads it (see `run`).
///
/// The group, not the child: `run` spawns `sh -c`, and a hung build is
/// normally something the shell started. Killing the shell alone leaves that
/// running against the release the deploy is about to give up on.
///
/// Failures are ignored throughout. This runs on the path where the build has
/// already been given up on, and a kill that cannot be delivered leaves
/// nothing this function can do about it that the caller has not already
/// decided.
async fn abandon(child: &mut tokio::process::Child, group: u32) {
    crate::shared::kill_group(group);
    // The group signal is the one that matters; this covers a build that
    // left its group before the signal landed.
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::test_support::build;
    use super::super::{BuildSpec, run};
    use super::*;
    use crate::fixtures;

    /// fails if a build that SUCCEEDS leaves its background job running.
    ///
    /// `sh -c` exiting is not the build being over. A command can leave a job
    /// behind, and this module has known that since round 4: it is named in
    /// `copy_artifact` as the thing every containment check is racing, and one
    /// was measured winning that race in 0.03s, swapping a component between a
    /// name being approved and the same name being opened.
    ///
    /// Every fix for that raced the job. None removed it. A build that exits 0
    /// with a job still running left it running, free to alter the release
    /// after the artifacts were validated and to outlive the deploy entirely.
    ///
    /// A process that means to persist calls `setsid` and leaves the group, so
    /// this kills a stray `&` and not a compiler daemon.
    #[tokio::test]
    async fn a_successful_build_does_not_leave_its_background_job_running() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("sleep 400 & echo $! > bg.pid".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };

        build(rel.path(), &spec).await.expect("this build exits 0");

        let pid = std::fs::read_to_string(rel.path().join("bg.pid"))
            .expect("the build wrote its background job's pid");
        assert!(
            !still_running(pid.trim()),
            "the background job (pid {}) outlived the build that started it",
            pid.trim()
        );
    }

    /// fails if a build that never finishes is waited on forever.
    ///
    /// It does not stop one target, it stops the dog. `crate::poll::tick`
    /// deploys targets one at a time, so a build that hangs holds that loop
    /// with no other target deploying, no smit refreshed and nothing logged,
    /// because from the loop's point of view nothing has failed. The same
    /// shape `git_timeout` exists to prevent for a fetch.
    ///
    /// Not `start_paused`, and it cannot be: see the note on the timeout in
    /// `run` for why a real subprocess and a paused clock cannot be combined.
    #[tokio::test]
    async fn a_build_that_never_finishes_is_abandoned() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            // `&` plus `wait` keeps the SHELL alive with a descendant. A bare
            // `sleep 600` is exec-replaced by the shell, so the child being
            // signalled would be `sleep` itself and `child.kill()` alone would
            // end it: the test would pass whether or not the group kill worked,
            // which makes it no test of the group at all.
            //
            // The descendant writes its own pid, so the assertion below can be
            // about the process rather than about the error. Asserting only
            // that the build timed out leaves the group kill exactly as
            // unproven as the bare `sleep 600` did.
            command: Some("sleep 600 & echo $! > descendant.pid; wait".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };

        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            fixtures::tempdir().path(),
            // Two seconds, not 250ms, for the reason `shep`'s own
            // `run_bounded` test learned the hard way: a budget this small is
            // shared with process startup, and on a loaded runner the shell
            // has not yet written its pid when the deadline fires. The
            // assertion below then reads a file that is not there and fails
            // for a reason that has nothing to do with process groups.
            //
            // Still three hundred times shorter than the `sleep 600` it has to
            // interrupt, so what the test is about is unchanged.
            Duration::from_secs(2),
        )
        .await
        .expect_err("a build that never finishes must be abandoned");

        assert!(
            matches!(err, Error::BuildTimedOut { .. }),
            "must say it timed out rather than that it failed: {err:?}"
        );
        assert!(
            format!("{err}").contains("build_timeout"),
            "must name the knob that changes it: {err}"
        );

        // The whole point of the process group. Killing the shell alone would
        // leave this running against a release the deploy has given up on.
        let pid = std::fs::read_to_string(rel.path().join("descendant.pid")).expect(
            "the build never got far enough to write its descendant's pid, so this \
             proves nothing about the group kill either way",
        );
        assert!(
            !still_running(pid.trim()),
            "the backgrounded descendant (pid {}) outlived the build it belonged to",
            pid.trim()
        );
    }

    /// Whether `pid` is still alive, polled rather than sampled once.
    ///
    /// Signalling a group and the kernel reaping its members are not the same
    /// instant, so one `kill -0` immediately after the build returns can catch
    /// a process that is already doomed and answer "alive". Bounded so a
    /// genuine leak still fails rather than hanging.
    ///
    /// `kill -0` because `#![forbid(unsafe_code)]` rules out the syscall, and
    /// its exit status is exactly the question being asked.
    fn still_running(pid: &str) -> bool {
        for _ in 0..50 {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .status()
                .expect("kill -0")
                .success();
            if !alive {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// fails if a failing build is treated as success. This is the guard
    /// that keeps a broken build from ever reaching the swap: current
    /// never moves and the running app is untouched, because it lives in a
    /// different directory.
    #[tokio::test]
    async fn a_failing_build_is_an_error() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("exit 3".into()),
            ..Default::default()
        };
        assert!(build(rel.path(), &spec).await.is_err());
    }
}
