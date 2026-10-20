//! Running `git` and other subprocesses, and running blocking work off the runtime.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::error::Error;

/// [`run_git`], abandoning the subprocess if it outlives `budget`.
///
/// Only [`crate::git::fetch`] uses this, and only because it is the one git
/// invocation that talks to a network. The other ten callers operate on local
/// directories and cannot hang on a remote that stopped answering.
///
/// Why it matters more than an ordinary slow call: the poll loop deploys
/// targets one at a time, so an unbounded fetch does not fail one target, it
/// stops every target and the smit refresh with it, with no error and no log
/// line. A remote behind a firewall that drops packets rather than refusing
/// them produces exactly that, and it is an ordinary misconfiguration.
///
/// The child is killed on expiry rather than left to finish, because a git
/// process still holding the bare clone's lock would fail the next tick too.
///
/// ## Why the pipes are drained on threads
///
/// A pipe holds about 64 KiB before a writer blocks in `write(2)`. Reading
/// only after the child exits therefore deadlocks any child that says more
/// than that: it blocks writing, `try_wait` reports it still running forever,
/// and the budget kills a process that was healthy and nearly finished.
///
/// Measured 2026-08-28 while reviewing this function's first version, which
/// had exactly that bug: a child writing 200 KB and then exiting was killed at
/// a three-second deadline, having been ready to exit in milliseconds. It
/// would have turned a `git fetch --prune` against a repository with many
/// refs into a target reported as an unreachable remote, which is a worse
/// failure than the hang this exists to prevent.
///
/// So each pipe gets a thread that reads to EOF, which is what
/// `wait_with_output` does internally and the reason it cannot be used here:
/// it also waits, and waiting is what this function needs to bound.
///
/// Residual, stated rather than hidden: this still occupies its caller for up
/// to `budget`, and the poll loop deploys targets one at a time, so a target
/// whose remote is a black hole delays the others by that much. It no longer
/// occupies the RUNTIME: every caller runs it through [`off_thread`], so a
/// stop, a refused handshake or the client's reconnect are all still served
/// while it waits. Bounded and reported beats unbounded and silent, which is
/// the whole of what this buys.
///
/// # Errors
/// [`Error::Git`] naming the command and a `None` status if `budget` elapses
/// first. Otherwise exactly what [`run_git`] returns.
pub(crate) fn run_git_within(dir: &Path, args: &[&str], budget: Duration) -> Result<String, Error> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so the whole tree can be signalled rather
        // than just git. See `abandon` for why that is the difference between
        // bounded and not.
        .process_group(0)
        .spawn()
        .map_err(Error::at(dir))?;

    // Taken before the loop so the child always has a reader, whatever the
    // loop then decides about the clock. See the doc above.
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Err(source) => {
                let _ = child.kill();
                return Err(Error::Io {
                    path: dir.to_owned(),
                    source,
                });
            }
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            abandon(&mut child);
            break None;
        }
        // Never sleep past the deadline: `POLL` is 50ms, and a budget smaller
        // than that would otherwise overshoot to 50ms whatever was asked for.
        thread::sleep(POLL.min(left));
    };

    let Some(status) = status else {
        // Returning WITHOUT joining, deliberately. The reader threads exist to
        // keep the child unblocked, and their output is not wanted on this
        // path. Joining here would reintroduce the unbounded wait this whole
        // function exists to remove: a grandchild holding the pipe means the
        // read end never sees EOF, and `abandon` signalling the group makes
        // that unlikely rather than impossible. They finish and drop on their
        // own when the last writer goes.
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: None,
            stderr: format!(
                "no answer within {budget:?}; abandoned so the other targets keep deploying"
            ),
        });
    };

    // Bounded, like the wait above and for the same reason. git exiting does
    // not mean the pipe is closed: anything it forked that inherited fd 1 or 2
    // still holds the write end, and an ssh ControlPersist master or a
    // `&`-backgrounded helper in an alias does exactly that while git itself
    // exits 0. Joining unconditionally here made the SUCCESS path the one
    // remaining unbounded wait, which is worse than the failure paths already
    // fixed, because it fires on the ordinary case.
    //
    // Measured 2026-08-28 before this change: a git alias forking
    // `sh -c 'sleep 8 &'` returned Ok after 8.02 seconds against a 600ms
    // budget, having reported success the whole time.
    // Each wait re-reads the clock. Computing the remaining time once and
    // spending it twice is how a two-pipe collect quietly doubles the budget:
    // if the first pipe consumes all of it, the second is handed a fresh copy.
    // Found while re-reading this function on 2026-08-28, having written the
    // same class of overrun into it three times already.
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let (Ok(stdout), Ok(stderr)) = (out.recv_timeout(remaining()), err.recv_timeout(remaining()))
    else {
        // NOT `abandon` here. That signals the process group by negating
        // `child.id()`, which is only safe while the child is alive: by this
        // point `try_wait` has reaped it, and the wait above can have taken
        // the whole budget, which the dog's `[deploy]` section allows to be minutes. A pid
        // recycled in that window would put a SIGKILL into an unrelated
        // process group, and the dog runs as root under the arrangement
        // shep's own docs recommend, so the usual same-uid check would not
        // stop it.
        //
        // Nothing is signalled instead: what holds the pipe is an orphan git
        // left behind, and this process has no safe way to name it.
        //
        // The readers are not simply abandoned, though. Dropping the receivers
        // is what lets them finish: their `send` then fails, which is the
        // documented shape, and the thread returns. Without that they would
        // sit in `read_to_end` for as long as the orphan lives, holding a
        // thread and a pipe fd each. The poll loop runs a fetch per target per
        // tick forever, so a remote that reliably produces such an orphan
        // would leak two of each per tick until the process ran out, taking
        // every other target down with it.
        drop(out);
        drop(err);
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: None,
            stderr: format!(
                "exited, but something it started still held its output after \
                 {budget:?}; abandoned so the other targets keep deploying"
            ),
        });
    };

    decode(dir, args, status, stdout, stderr)
}

/// Kills a whole process group by the leader's pid.
///
/// One spelling, shared, because two are what drift. `crate::build` abandons a
/// build the same way and cannot share the rest: its child is a
/// `tokio::process::Child` and this one is a `std::process::Child`, so the
/// fallback differs while the signal and the group form must not.
///
/// `killpg(2)` through `nix` rather than spawning `kill`. The spawn was there
/// because the crate forbids unsafe and `libc::killpg` is unsafe; `nix` wraps
/// the same call safely and is already a dependency. It also removes the one
/// place this crate ran a binary found on `PATH` other than `git`, and the
/// fork it cost on every abandoned build.
///
/// The failure is ignored on purpose. Every caller is already giving up on the
/// child, and a kill that cannot be delivered leaves nothing they can do about
/// it that they have not already decided. A pid too large for `pid_t` is the
/// same case: no such group can exist.
pub(crate) fn kill_group(pid: u32) {
    // Zero is not a group either: `killpg(0)` signals the CALLER's group,
    // which is this dog and every build it has running.
    if pid == 0 {
        return;
    }
    if let Ok(pid) = i32::try_from(pid) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

/// Kills a timed-out child and everything it spawned, then reaps it.
///
/// The group, not just the pid. `Child::kill` signals the immediate process
/// only, and `git fetch` over `ssh://` forks an `ssh` that inherits our pipe
/// write-ends. Killing git alone leaves that `ssh` holding the pipe open, so
/// the read end never sees EOF. Demonstrated 2026-08-28: a reader thread on a
/// pipe a grandchild still holds does not finish, so joining it would hang
/// past the budget, which is the exact failure this function exists to
/// prevent.
///
/// Reaped afterwards: leaving a zombie git would keep the bare clone's lock
/// and fail the next tick for a reason that looks unrelated.
fn abandon(child: &mut std::process::Child) {
    kill_group(child.id());
    // The group signal is the one that matters; this covers a child that
    // left its group before the signal landed.
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads one of a child's pipes to EOF on its own thread.
///
/// A free function rather than a closure because the two pipes are different
/// types and a closure cannot be generic over them.
fn drain<R: io::Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        // The receiver is dropped when the budget runs out, so this send can
        // fail. That is the intended shape, not an error to report.
        let _ = tx.send(buf);
    });
    rx
}

/// Runs `work` on tokio's blocking pool and answers with its result.
///
/// Every git call in this crate is a blocking `std::process::Command`, and
/// the artifact copy is blocking I/O. Run on the runtime's one thread, each
/// stalled everything else polled on it for its whole duration: a five
/// minute fetch against a remote that drops packets meant five minutes in
/// which a `SIGTERM` went unanswered, a refused handshake went unnoticed and
/// the client's reconnect could not run. On the pool, the runtime keeps
/// turning and the caller alone waits.
///
/// A panic inside `work` is re-raised here rather than turned into an
/// error, so it reaches the poll loop's own guard and is reported as the
/// bug it is. The only other way a blocking task ends without a result is
/// the runtime shutting down underneath it, which is the process ending.
///
/// # Errors
/// Whatever `work` returns.
pub(crate) async fn off_thread<T, F>(work: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Error> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(join) => match join.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(join) => panic!("a blocking task was cancelled: {join}"),
        },
    }
}

/// How often [`run_git_within`] asks whether the child has finished.
///
/// Fifty milliseconds: short enough that a fetch finishing is not perceptibly
/// delayed, long enough that a five-minute budget costs six thousand cheap
/// syscalls rather than a busy loop.
const POLL: Duration = Duration::from_millis(50);

/// Runs `git <args>` in `dir` and returns its stdout as a `String`.
///
/// `pub(crate)` rather than private: [`crate::git`] shells out to git for a
/// second directory this crate cares about - the bare clone under
/// [`crate::paths::Tree::git`] - and needs the exact same error mapping this
/// module already built, so it reuses this rather than growing a second
/// copy. The parameter is named `dir`, not `checkout`, because it is called
/// with both: this module's own functions always pass the operator's
/// checkout, `crate::git`'s pass the deploy engine's own bare clone.
///
/// Launching a subprocess and decoding what it printed are not filesystem
/// calls, but they fail with the same shape of error - an
/// [`std::io::Error`] and a path worth naming - and this crate has nowhere
/// else for that shape to live, so both come back as [`Error::Io`] naming
/// `dir`. A `git` invocation that launches but exits non-zero is
/// [`Error::Git`] instead, since `git`'s own stderr is worth keeping
/// separate from "could not even run it".
///
/// Unbounded on purpose: see [`run_git_within`] for the one caller that
/// cannot afford that, and why the other ten can.
///
/// # Errors
/// [`Error::Io`] naming `dir` if git cannot be launched or printed something
/// that is not UTF-8. [`Error::Git`] if it ran and exited non-zero.
pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(Error::at(dir))?;

    decode(dir, args, output.status, output.stdout, output.stderr)
}

/// Turns a finished `git` invocation into stdout or the error it earned.
///
/// Shared by [`run_git`] and [`run_git_within`] rather than duplicated: the
/// two differ only in how they wait for the child, and a second copy of this
/// is how the bounded path would drift into reporting failures differently
/// from the unbounded one.
///
/// Takes the buffers by value because both callers own an `Output` they never
/// look at again, so borrowing would only force a copy of stdout back out.
///
/// # Errors
/// [`Error::Git`] if `status` is non-zero, carrying git's own stderr.
/// [`Error::Io`] naming `dir` if stdout is not UTF-8.
fn decode(
    dir: &Path,
    args: &[&str],
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<String, Error> {
    if !status.success() {
        return Err(Error::Git {
            command: format!("git {}", args.join(" ")),
            status: status.code(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }

    String::from_utf8(stdout).map_err(|err| Error::Io {
        path: dir.to_owned(),
        source: io::Error::other(err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `off_thread` stops passing a result through, or turns a
    /// panic into an error. The poll loop's guard is what should see a
    /// panic, so it has to leave here as one.
    #[tokio::test]
    async fn off_thread_answers_with_the_result_and_re_raises_a_panic() {
        let answered = off_thread(|| Ok::<_, Error>(41 + 1))
            .await
            .expect("passes through");
        assert_eq!(answered, 42);

        let refused = off_thread(|| Err::<(), _>(Error::Config("no".to_owned())))
            .await
            .expect_err("passes the error through");
        assert!(matches!(refused, Error::Config(_)));

        let caught = std::panic::AssertUnwindSafe(off_thread(|| -> Result<(), Error> {
            panic!("inside the pool");
        }));
        let outcome = futures_catch(caught).await;
        assert!(outcome.is_err(), "the panic must come out as a panic");
    }

    /// `catch_unwind` for a future, the same dozen lines `crate::poll`
    /// keeps, spelled here so this module's test does not reach into that
    /// one's.
    async fn futures_catch<F: std::future::Future>(
        fut: std::panic::AssertUnwindSafe<F>,
    ) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        struct Catch<F>(Pin<Box<F>>);
        impl<F: Future> Future for Catch<F> {
            type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let inner = &mut self.get_mut().0;
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    inner.as_mut().poll(cx)
                })) {
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
                    Err(payload) => Poll::Ready(Err(payload)),
                }
            }
        }
        Catch(Box::pin(fut.0)).await
    }

    /// fails if a chatty-but-healthy git subprocess is killed as if it hung.
    ///
    /// A pipe holds about 64 KiB before a writer blocks. The first version of
    /// `run_git_within` read the pipes only after `try_wait` reported the
    /// child exited, so a child saying more than that blocked in `write(2)`,
    /// never exited, and was killed at the deadline having done nothing wrong.
    ///
    /// Measured 2026-08-28 against that version: a child writing 200 KB and
    /// then exiting was killed at a three-second deadline, when reading its
    /// pipe would have let it finish in milliseconds. `git fetch --prune`
    /// against a repository with many refs says far more than 64 KiB, so this
    /// would have reported healthy remotes as unreachable ones: a worse
    /// failure than the hang the budget exists to prevent.
    ///
    /// `git ls-remote` on the crate's own repository is the chatty subject
    /// because it is guaranteed local, needs no network, and prints one line
    /// per ref.
    #[test]
    fn a_chatty_subprocess_is_not_mistaken_for_a_hung_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q", "-b", "main"]).expect("init");
        run_git(dir.path(), &["config", "user.email", "t@example.invalid"]).expect("email");
        run_git(dir.path(), &["config", "user.name", "t"]).expect("name");
        // Comfortably past a 64 KiB pipe buffer, and one write rather than
        // thousands of git invocations.
        std::fs::write(dir.path().join("big.txt"), "x".repeat(400_000)).expect("big file");
        run_git(dir.path(), &["add", "big.txt"]).expect("add");
        run_git(dir.path(), &["commit", "-q", "-m", "big"]).expect("commit");

        let out = run_git_within(
            dir.path(),
            &["show", "HEAD:big.txt"],
            Duration::from_secs(20),
        )
        .expect("a chatty command must not be mistaken for a hung one");
        assert!(
            out.len() > 100_000,
            "the fixture must exceed a pipe buffer or this proves nothing, got {} bytes",
            out.len()
        );
    }

    /// fails if a process git spawned can extend the budget on the SUCCESS
    /// path, where git itself exited cleanly.
    ///
    /// The nastiest of the three versions of this bug, because it fires on the
    /// ordinary case rather than on a failure. `git` can exit 0 while
    /// something it forked still holds fd 1 or 2: an ssh ControlPersist
    /// master, or a `&`-backgrounded helper in an alias. `try_wait` sees
    /// success, the loop breaks, and collecting the output blocks on a pipe
    /// that will not close.
    ///
    /// Measured 2026-08-28 before the fix: this exact shape returned `Ok`
    /// after 8.02 seconds against a 600ms budget.
    #[test]
    fn output_is_collected_within_the_budget_even_when_git_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Instant::now();
        let _ = run_git_within(
            dir.path(),
            &["-c", "alias.fork=!sh -c 'sleep 8 &'", "fork"],
            Duration::from_millis(600),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(4),
            "git exited at once; collecting its output must not wait out what it \
             forked. Took {elapsed:?} against a 600ms budget"
        );
    }

    /// fails if a process git spawned can extend the budget past its end.
    ///
    /// `Child::kill` signals the immediate pid only. `git fetch` over `ssh://`
    /// forks an `ssh` that inherits our pipe write-ends, so killing git alone
    /// leaves that `ssh` holding the pipe and the read end never sees EOF.
    /// Demonstrated 2026-08-28: a reader thread on a pipe a grandchild still
    /// holds does not finish, so joining it hangs past the budget, which is
    /// the exact failure this function exists to prevent.
    ///
    /// Uses `sh` rather than git because git is hard to make fork on demand,
    /// and the mechanism under test is the process group, not git.
    ///
    /// The assertion is on ELAPSED TIME, not on the error. An error alone
    /// would be returned by the broken version too, eventually; the whole
    /// claim is that it comes back near the budget rather than near the
    /// grandchild's own lifetime.
    #[test]
    fn a_grandchild_cannot_extend_the_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `git -c alias.x=!<shell>` is how you make git fork something that
        // outlives it while still being a git invocation.
        let started = Instant::now();
        let err = run_git_within(
            dir.path(),
            &["-c", "alias.hang=!sh -c 'sleep 30 &' && sleep 30", "hang"],
            Duration::from_millis(600),
        )
        .expect_err("must not succeed");
        let elapsed = started.elapsed();

        assert!(
            format!("{err}").contains("no answer within"),
            "must fail on the budget: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must return near the budget, not wait out the grandchild; took {elapsed:?}"
        );
    }

    /// fails if a git subprocess can outlive its budget.
    ///
    /// Without the bound this hangs forever rather than failing, which is the
    /// whole point: the poll loop deploys targets one at a time, so an
    /// unanswered fetch stops every target with no error and no log line.
    ///
    /// `10.255.255.1` is RFC 1918 space that routes nowhere on an ordinary
    /// host, so the connect blocks rather than being refused. Measured before
    /// this test was written: the same fetch was still blocked after three
    /// seconds. A refusal would make this test pass for the wrong reason, so
    /// it asserts on the timeout's own message rather than merely on `Err`.
    #[test]
    fn a_git_subprocess_that_never_answers_is_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q"]).expect("init");

        let err = run_git_within(
            dir.path(),
            &[
                "fetch",
                "git://10.255.255.1/x",
                "+refs/heads/*:refs/heads/*",
            ],
            Duration::from_millis(400),
        )
        .expect_err("an unanswered fetch must not hang");

        let said = format!("{err}");
        assert!(
            said.contains("no answer within"),
            "must fail on the budget, not on something else: {said}"
        );
    }
}
