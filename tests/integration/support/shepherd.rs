//! Drives a real shepherd and a real `shep-deploy`: the tier's helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The `shep-deploy` binary under test, as cargo built it for this run.
pub const DEPLOY_BIN: &str = env!("CARGO_BIN_EXE_shep-deploy");

/// How long any "wait until the world catches up" poll gets before it gives
/// up and fails the test.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// The `shep` binary under test.
///
/// # Panics
/// If `$SHEP_BIN` is unset or does not point at a file. Loudly, rather than
/// skipping: a tier that quietly does nothing is the failure mode this whole
/// file exists to avoid.
pub fn shep_bin() -> PathBuf {
    let raw = std::env::var("SHEP_BIN").expect(
        "the integration tier needs $SHEP_BIN pointing at a built shep binary, for example \
         SHEP_BIN=\"$(command -v shep)\"",
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "$SHEP_BIN does not name a file: {}",
        path.display()
    );
    path
}

/// One shepherd in its own temporary `$SHEP_HOME`, killed on drop.
pub struct Shepherd {
    home: tempfile::TempDir,
    shep: PathBuf,
}

impl Shepherd {
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("a temporary $SHEP_HOME");
        // A unix socket path is bounded by the kernel: 104 bytes on macOS,
        // 108 on Linux. `$TMPDIR` is long on macOS, so this is close enough
        // to the limit to be worth saying out loud rather than discovering
        // as "the daemon process exited before it started answering".
        let socket = home.path().join("run/shep.sock");
        assert!(
            socket.as_os_str().len() < 100,
            "$TMPDIR is too deep for a unix socket here: {} is {} bytes and the kernel allows \
             about 104. Run with a shorter TMPDIR.",
            socket.display(),
            socket.as_os_str().len()
        );
        Self {
            home,
            shep: shep_bin(),
        }
    }

    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// Run one `shep` command against this home and hand back its output.
    ///
    /// `SHEP_HOME` goes in the environment as well as in `--home`, matching
    /// `shep-log-rotate`'s own integration tier: `shep adopt` vets a
    /// candidate binary by spawning it with this process's environment
    /// inherited, and a missing `SHEP_HOME` there would point that spawn at
    /// whatever `$SHEP_HOME` this test process itself has.
    pub fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.shep)
            .args(args)
            .arg("--home")
            .arg(self.home())
            .env("SHEP_HOME", self.home())
            .output()
            .expect("shep ran")
    }

    /// Run one `shep` command and require it to succeed.
    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "shep {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run `shep-deploy deploy <sheep>` against this home, exactly as the
    /// supervised dog or an operator's direct invocation would.
    pub fn deploy(&self, sheep: &str) -> Output {
        self.deploy_args(&["deploy", sheep])
    }

    /// Run `shep-deploy` against this home with whatever argv is given.
    ///
    /// `SHEP_HOME` and nothing else. A verb needing more than that would
    /// work here and fail under the shepherd, which hands a dog `SHEP_HOME`
    /// and `SHEP_DOG_NAME` and no argv at all.
    ///
    /// `SHEP_DOG_NAME` is deliberately absent: these are the one-shot verbs,
    /// and the whole point of reading that variable is that a command run by
    /// hand is not a dog and must not announce itself as one. See
    /// `adopted_as` in `src/main.rs`.
    pub fn deploy_args(&self, args: &[&str]) -> Output {
        Command::new(DEPLOY_BIN)
            .args(args)
            .env("SHEP_HOME", self.home())
            .output()
            .expect("shep-deploy ran")
    }
}

impl Drop for Shepherd {
    fn drop(&mut self) {
        // Failures ignored: a test that already failed must report its own
        // reason, not this one.
        let _ = self.run(&["kill", "--style", "bare"]);
    }
}

/// The last non-empty line of `path`, with shep's timestamp taken back off,
/// or `None` if it cannot be read yet.
///
/// The app's stdout log is how these tests tell which RELEASE is executing,
/// which `shep describe` cannot answer: it reports status and pid, not code.
/// The last line specifically, because the log accumulates across reloads -
/// including the short-lived instance of a release that failed to come up.
///
/// Through `shep_core::logstamp::strip`, the same call `shep bleats` makes
/// when it reads one of these files, so what a caller compares against is what
/// an operator is shown rather than a prefix this test learned to expect. shep
/// stamps every line it writes to a log FILE as of 0.1.28; the bus feed a
/// follower reads is unstamped. `strip` recognises a stamp by parsing it, so a
/// line that never carried one comes back unchanged and this stays correct
/// against an older shep too.
pub fn last_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .rfind(|line| !line.trim().is_empty())
        .map(|line| shep_client::shep_core::logstamp::strip(line).to_owned())
}

/// Poll `ready` until it answers true, or fail with `what`.
pub fn wait_until(what: &str, ready: impl Fn() -> bool) {
    wait_within(what, PATIENCE, ready);
}

/// As [`wait_until`], but with a budget the caller chose because the budget
/// is itself the assertion.
///
/// [`PATIENCE`] is generous on purpose - it exists so a slow machine does
/// not turn a passing test red. A wait that is measuring HOW LONG something
/// took needs the opposite, so it gets its own number and a comment saying
/// what the number is against.
pub fn wait_within(what: &str, budget: Duration, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Whether `shep describe` reports `sheep` as `online`.
///
/// A pid alone is not enough for the probeless tests: shep hands out a pid
/// the moment it spawns and only calls the sheep `online` once its readiness
/// path is satisfied, which for those is a whole `listen_timeout` later.
pub fn described_online(shepherd: &Shepherd, sheep: &str) -> bool {
    shepherd
        .ok(&["describe", sheep, "--format", "json"])
        .contains("\"status\":\"online\"")
}

/// The pid `shep describe` reports for `sheep`, or `None` if it has none or
/// cannot be found at all.
pub fn described_pid(shepherd: &Shepherd, sheep: &str) -> Option<u32> {
    let listing = shepherd.ok(&["describe", sheep, "--format", "json"]);
    // No JSON dependency in this crate - a pid is the only field these tests
    // need, and it is unambiguous to find as text: `"pid":1234`.
    listing
        .split("\"pid\":")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// How many instances `shep describe` reports under `sheep`.
///
/// Counted as `"id":` occurrences, the same shape [`described_pid`] reads a
/// pid with and for the same reason - no JSON dependency in this crate. One
/// per instance and nothing else in the listing carries that key: a lamb has
/// a pid and a name, never an id.
pub fn described_instances(shepherd: &Shepherd, sheep: &str) -> usize {
    shepherd
        .ok(&["describe", sheep, "--format", "json"])
        .matches("\"id\":")
        .count()
}

/// The stdout log shep is writing for `sheep` right now, as `describe`
/// itself names it.
///
/// Read from the listing rather than assembled from a slot number, because
/// the slot is not predictable across a cutover. Measured on a real
/// shepherd: `Start` on a registered name adds the newcomer BESIDE the
/// original, so the newcomer takes slot 1 and keeps it after the original is
/// deleted. A helper hard-coding `<sheep>-0-out.log` would go on reading the
/// log of the instance the cutover removed, whose last line is whatever it
/// was serving when it died.
///
/// A later reload does not move it - the instance id changes and the slot
/// does not - so one read before a deploy is good for the assertions after
/// it.
pub fn out_file(shepherd: &Shepherd, sheep: &str) -> PathBuf {
    let listing = shepherd.ok(&["describe", sheep, "--format", "json"]);
    let named = listing
        .split("\"out_file\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("describe names an out_file");
    PathBuf::from(named)
}
