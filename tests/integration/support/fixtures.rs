//! Fixture repositories, Flockfiles, deploy trees and registrations.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use super::shepherd::Shepherd;

/// Runs a git subcommand for fixture setup, panicking if it fails.
#[track_caller]
pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// `dir`'s current `HEAD` sha.
#[track_caller]
pub fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    // Checked, because the failure is silent otherwise: a repository with no
    // commits gives an empty stdout and a status nobody reads, so the caller
    // gets `""` and asserts against it. Same check as `src/fixtures.rs`'s
    // copy, which this one had drifted behind.
    assert!(
        out.status.success(),
        "git rev-parse HEAD failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8(out.stdout)
        .expect("utf-8 sha")
        .trim()
        .to_owned()
}

/// The `web` app as both Flockfiles here declare it: script, and a
/// readiness probe that looks THROUGH the `current` symlink for a file a
/// release either ships or does not.
///
/// `with_cwd` is the one difference between the two copies, and it is not
/// cosmetic - see [`register_web`].
///
/// Every test in this tier needs a probe, because `Verify::Probed` refuses
/// a target without one and refuses it by reading the RELEASE's Flockfile,
/// which is the only app definition the dog can see: `describe` reports
/// status and pid, never the config shep registered. The two copies can
/// therefore disagree, and this fixture keeps them honest by generating
/// both from here.
pub fn app_toml(home: &Path, with_cwd: bool, readiness: Readiness, extra: &str) -> String {
    let current = home.join("deploy/web/current");
    let cwd = if with_cwd {
        format!("cwd = {:?}\n", current.to_str().expect("utf-8 path"))
    } else {
        String::new()
    };
    let gate = match readiness {
        Readiness::Probe => probe_toml(home),
        Readiness::Heuristic(listen) => format!("listen_timeout = \"{listen}s\"\n"),
    };
    format!("[[app]]\nname = \"web\"\nscript = \"./run.sh\"\n{cwd}{extra}{gate}")
}

/// The exec probe every probed fixture uses: `test -f` on a marker reached
/// THROUGH `current`, so a release that ships the marker can become ready
/// and one that does not never can.
pub fn probe_toml(home: &Path) -> String {
    let current = home.join("deploy/web/current");
    format!(
        "\n[app.readiness_probe]\nkind = \"exec\"\ntarget = \"test -f \
         {marker}\"\ninterval = \"1s\"\ntimeout = \"2s\"\nfailure_threshold = 1\n",
        marker = current.join("ready-marker").display(),
    )
}

/// Writes the app into `dir` the way an operator's repository carries it:
/// `Flockfile.toml` with the script and nothing the dog refuses in a
/// committed file, and, for a probed app, the probe in
/// `Flockfile.override.toml` with a `.gitignore` that keeps it out of the
/// commit.
///
/// The split is the dog's own rule, not a test convenience: an `exec` probe
/// is run by the shepherd at its own uid and ignores `user`, so a committed
/// one is refused, and the override is where an operator puts theirs. The
/// deploy links the ignored override in from the checkout, which for these
/// fixtures is `dir` itself, and honours the probe from there.
pub fn write_committed_app(dir: &Path, home: &Path, readiness: Readiness, extra: &str) {
    let gate = match readiness {
        Readiness::Probe => String::new(),
        Readiness::Heuristic(listen) => format!("listen_timeout = \"{listen}s\"\n"),
    };
    fs::write(
        dir.join("Flockfile.toml"),
        format!("[[app]]\nname = \"web\"\nscript = \"./run.sh\"\n{extra}{gate}"),
    )
    .expect("write Flockfile");
    if matches!(readiness, Readiness::Probe) {
        fs::write(dir.join(".gitignore"), "Flockfile.override.toml\n").expect("write .gitignore");
        fs::write(
            dir.join("Flockfile.override.toml"),
            format!("[[app]]\nname = \"web\"\n{}", probe_toml(home)),
        )
        .expect("write the operator's override");
    }
}

/// How a test's app reports itself ready.
#[derive(Clone, Copy)]
pub enum Readiness {
    /// An exec probe through `current`, which is what the deploy sequence
    /// wants and what `verify = "probed"` requires.
    Probe,
    /// Nothing at all, which is what most real apps have. shep falls back
    /// to sleeping the whole of the `listen_timeout` carried here, per
    /// instance, and that is the case the verification window has to be
    /// derived for.
    Heuristic(u64),
}

/// A bare git origin with one committed app named `web`, whose script echoes
/// `version` to stdout and then sleeps, and which carries the marker file
/// the readiness probe looks for.
///
/// The version marker is how a test tells "the reload actually ran the new
/// release" apart from "the old process is still running" - both look
/// identical from `shep describe` alone, since `describe` only reports
/// status and pid, not which release's code is executing.
///
/// `ready-marker` is the other half. The probe is `test -f
/// <home>/deploy/web/current/ready-marker`, so it resolves through the
/// symlink the deploy swaps: a release that ships the marker can become
/// ready and one that does not never can. That is how a test makes a real
/// shepherd's `AwaitReady` fail on purpose, which is the only way to
/// exercise a rollback end to end.
pub fn origin_with_app(
    home: &Path,
    version: &str,
    readiness: Readiness,
    extra: &str,
) -> tempfile::TempDir {
    let origin = tempfile::tempdir().expect("tempdir");
    git(origin.path(), &["init", "-q", "-b", "main"]);
    git(origin.path(), &["config", "user.email", "test@example.com"]);
    git(origin.path(), &["config", "user.name", "test"]);
    write_committed_app(origin.path(), home, readiness, extra);
    fs::write(origin.path().join("ready-marker"), "").expect("write ready-marker");
    write_run_script(origin.path(), version);
    git(origin.path(), &["add", "."]);
    git(origin.path(), &["commit", "-q", "-m", version]);
    origin
}

/// Overwrites `dir`'s `run.sh` to echo `version` before sleeping.
pub fn write_run_script(dir: &Path, version: &str) {
    write_script(dir, &format!("#!/bin/sh\necho {version}\nsleep 300\n"));
}

/// As [`write_run_script`], but for an app that uses its whole drain
/// window: it ignores `SIGTERM` and keeps running until shep's
/// `graceful_timeout` expires and the `SIGKILL` lands.
///
/// The loop matters as much as the trap. `trap '' TERM` makes the shell
/// itself ignore the signal, but a `sleep 300` child that is signalled too
/// would end the script early and the drain with it; re-sleeping in a loop
/// means the only thing that can end this process is the kill.
pub fn write_stubborn_run_script(dir: &Path, version: &str) {
    write_script(
        dir,
        &format!("#!/bin/sh\ntrap '' TERM\necho {version}\nwhile :; do sleep 1; done\n"),
    );
}

/// Writes `body` as `dir/run.sh`, executable.
pub fn write_script(dir: &Path, body: &str) {
    let path = dir.join("run.sh");
    fs::write(&path, body).expect("write run.sh");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// A deploy tree for `sheep`, at `home/deploy/<sheep>`, matching
/// `crate::paths::Tree`'s own layout: `git` fetched from `origin`, one
/// release built for `origin`'s current head, and `current` pointed at it.
///
/// Returns the sha that release was built for.
pub fn build_tree(home: &Path, sheep: &str, origin: &Path) -> String {
    let root = home.join("deploy").join(sheep);
    let git_dir = root.join("git");
    fs::create_dir_all(&git_dir).expect("create git dir");
    git(&git_dir, &["init", "-q", "--bare"]);

    let remote = origin.to_str().expect("utf-8 origin path").to_owned();
    git(
        &git_dir,
        &["fetch", "--prune", &remote, "+refs/heads/*:refs/heads/*"],
    );
    let sha = head_of(origin);

    let release = root.join("releases").join(&sha);
    git(
        &git_dir,
        &["worktree", "add", release.to_str().unwrap(), &sha],
    );
    symlink(&release, root.join("current")).expect("symlink current");

    sha
}

/// Writes `deploy.toml` for `sheep`, tracking `origin`'s `main` branch,
/// already deployed at `sha`, and verifying the way `verify` says.
///
/// Field order and shape matches `crate::state::State`'s own TOML. `watch`
/// is left unset so it takes its documented default (`Auto`), the same
/// round trip `src/state.rs`'s own tests pin.
pub fn write_state(home: &Path, sheep: &str, origin: &Path, sha: &str, verify: &str) {
    let path = home.join("deploy").join(sheep).join("deploy.toml");
    fs::write(
        &path,
        format!(
            "remote = {remote:?}\nbranch = \"main\"\ndeployed = {sha:?}\nverify = \
             {verify:?}\ncheckout = {remote:?}\n",
            remote = origin.to_str().expect("utf-8 origin path"),
        ),
    )
    .expect("write deploy.toml");
}

/// Registers `web` with the shepherd, from a Flockfile OUTSIDE any release,
/// whose `cwd` is the `current` symlink.
///
/// **`cwd` is set explicitly, and to the symlink.** A Flockfile app whose
/// `cwd` is left to default takes its own directory - and shep resolves
/// that to a real path when it registers the app. Registering from
/// `<home>/deploy/web/current/Flockfile.toml` therefore pins the sheep to
/// the release that happened to be current at registration, and every later
/// deploy swaps a symlink the running app no longer reaches. Measured on a
/// real shepherd: the reload after a swap re-ran the OLD release's script.
/// An explicit `cwd` is stored verbatim, symlink and all, which is what the
/// design means by "the sheep's `cwd` is this path, permanently".
///
/// **The probe is registered, not deployed.** Nothing in this crate ever
/// re-registers the app, so the probe a reload actually uses is this one
/// and not whatever the new release's Flockfile says. A test that wants a
/// reload to fail therefore cannot change the probe; it changes what the
/// probe LOOKS at, which is why the probe points through `current` at a
/// file a release either ships or does not.
pub fn register_web(shepherd: &Shepherd, readiness: Readiness, extra: &str) {
    register_web_with_settings(shepherd, readiness, extra, "");
}

/// [`register_web`] with `settings` as the body of the sheep's
/// `[app.dogs.deploy]` table, which is where the dog reads its build command
/// from. The release's Flockfile is not consulted for it.
pub fn register_web_with_settings(
    shepherd: &Shepherd,
    readiness: Readiness,
    extra: &str,
    settings: &str,
) {
    let path = shepherd.home().join("register.toml");
    let table = if settings.is_empty() {
        String::new()
    } else {
        format!("\n[app.dogs.deploy]\n{settings}")
    };
    fs::write(
        &path,
        format!(
            "{}{table}",
            app_toml(shepherd.home(), true, readiness, extra)
        ),
    )
    .expect("write register.toml");
    shepherd.ok(&[
        "start",
        path.to_str().expect("utf-8 path"),
        "--style",
        "bare",
    ]);
}
