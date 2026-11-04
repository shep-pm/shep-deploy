//! The two questions shep asks this binary before it ever adopts or starts
//! it: `--version` and `--schema`.
//!
//! A separate target from `integration/` because it needs no shepherd. The
//! whole point of the contract is that shep can ask a dog that has never run
//! and cannot connect to anything, so a test that required a live daemon
//! would be testing something else.
//!
//! `src/config.rs` covers the schema's own shape. What is left to this file
//! is the part only a real process can answer: that `main` reaches the probe
//! at all, before the runtime is built and before the socket is opened.

use std::process::{Command, Output};

/// Runs the built binary with one argument, in an environment with no
/// `SHEP_HOME` and no shepherd to find.
///
/// The cleared environment is the assertion, not the setup. shep runs
/// `--version` against the binary on disk while the dog it is about to
/// restart is still connected, so a probe that reached for a socket would
/// either hang or do a poll loop's worth of work before shep killed its
/// process group.
fn probe(flag: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shep-deploy"))
        .arg(flag)
        .env_remove("SHEP_HOME")
        .env_remove("SHEP_DOG_NAME")
        .output()
        .expect("the test binary's own sibling is on disk")
}

/// fails if this dog stops answering `--version`, or answers something
/// shep's own parser cannot read.
///
/// Parsed with `parse_version_answer` rather than compared as a string,
/// because the format is only ever interesting to that parser. The number
/// that matters is the protocol: shep refuses an adopt below its floor and
/// warns on a restart, and a dog that answers nothing is recorded as
/// "protocol unknown" with nothing on screen saying so.
#[test]
fn the_version_answer_is_the_one_shep_reads() {
    let output = probe("--version");
    assert!(output.status.success(), "{:?}", output.status);

    let text = String::from_utf8(output.stdout).expect("the answer is text");
    let answer = shep_client::shep_core::dogs::parse_version_answer(&text)
        .expect("the answer shep's own parser cannot read is the bug this pins");

    assert_eq!(answer.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        answer.protocol,
        Some(shep_client::PROTOCOL_VERSION),
        "the protocol answered is the one this binary was compiled against"
    );
}

/// fails if this dog stops answering `--schema`, or answers something that
/// is not JSON.
///
/// Both are quiet failures rather than loud ones. Silence is the ordinary
/// case for a dog written before the contract, so shep adopts and says
/// nothing; a non-JSON answer earns one notice and an adopt either way. The
/// operator's first sign of either is a settings pane that is not there.
#[test]
fn the_schema_answer_is_json_describing_this_dogs_own_section() {
    let output = probe("--schema");
    assert!(output.status.success(), "{:?}", output.status);

    let schema: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the answer is JSON");

    assert_eq!(
        schema.get("title").and_then(serde_json::Value::as_str),
        Some("deploy"),
        "the pane heads its form with this, so it names the section rather than the Rust type"
    );
    assert!(
        schema.pointer("/properties/interval").is_some(),
        "the section's own keys are what the schema describes"
    );

    // The two marks `shep lookout` draws on: a key only `[deploy]` may set,
    // and a field whose value the pane must never print. `src/settings.rs`
    // checks the same marks on the schema it derives; what is pinned here is
    // that the answer this binary publishes carries them.
    for key in [
        "interval",
        "retention",
        "git_timeout",
        "build_timeout",
        "passthrough",
    ] {
        assert_eq!(
            schema
                .pointer(&format!("/properties/{key}/x-shep-dog-wide"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "`{key}` is dog-wide, so the pane must not offer it per sheep"
        );
    }
    assert_eq!(
        schema
            .pointer("/$defs/HookSpec/properties/env/x-shep-secret")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "a hook's env is where an operator keeps a password"
    );
}

/// fails if the manifest stops asking for the permissions this dog uses.
///
/// Since shep 0.21 a dog's grants are what its manifest asked for, and the
/// shepherd refuses any request outside them. A manifest that asks for
/// nothing adopts cleanly and then fails every tick on its first
/// `sheep_config`, with the refusal only in the dog's own log.
///
/// `secrets.read:production` is the one entry an operator has to read twice,
/// and it is the one that was missing on 2026-11-03: the read of a sheep's
/// table that resolves its `{{secret:...}}` references takes `secrets.read`
/// scoped to the environment that sheep resolves in, so a dog that never
/// asked for it was refused on every target of every tick. The gate is only
/// on a dog's own connection, so the integration tier's one-shot deploys
/// cannot see this missing - only the supervised dog's tests can, and they
/// fail as a timeout rather than as a refusal.
#[test]
fn the_manifest_asks_for_the_flock_permissions_the_dog_uses() {
    let output = probe(shep_client::shep_core::dogs::MANIFEST_FLAG);
    assert!(output.status.success(), "{:?}", output.status);

    let manifest: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the answer is JSON");
    let asked: Vec<(&str, bool)> = manifest["permissions"]
        .as_array()
        .expect("the manifest lists permissions")
        .iter()
        .map(|p| (p["name"].as_str().expect("named"), p["required"] == true))
        .collect();

    assert_eq!(
        asked,
        [
            ("flock.read", true),
            ("flock.control", true),
            ("flock.edit", true),
            // Scoped, because the permission is: `secrets.read` in one named
            // environment, and `production` is the shepherd's default. A flock
            // that changed `[daemon] environment` needs the matching scope
            // granted by hand, and `poll::a_supervised_dog_runs_a_sheeps_hook_with_its_secret_resolved`
            // is what fails if this line goes missing.
            ("secrets.read:production", true)
        ]
    );

    // The dog's `[deploy]` section is a settings layer for the sheep `init`
    // took over, not for every sheep in the flock. `Serves::OptedIn` is the
    // default and is skipped on the wire, so its absence is the declaration:
    // a flip to `EverySheep` would print `"serves": "every-sheep"` here and
    // make this fail.
    assert!(
        manifest.get("serves").is_none(),
        "the manifest offers this dog's settings to opted-in sheep only: {manifest}"
    );
}

/// fails if an ordinary run starts answering a probe, or a probe starts
/// falling through into one.
///
/// `route` reads a leading `-` as something that is not a sheep name, so
/// both flags landed on the usage message and exit 2 before the probe
/// existed. That is what shep read as a dog with no protocol and no schema,
/// and it is a state this crate can return to by moving one line.
#[test]
fn an_ordinary_argument_is_not_a_probe() {
    let output = probe("--not-a-flag-this-dog-knows");

    assert!(
        !output.status.success(),
        "an unknown flag is still a usage error"
    );
    assert!(
        output.stdout.is_empty(),
        "nothing is printed to stdout, so shep cannot read a usage message as an answer"
    );
}

/// Runs the built binary by hand: the given arguments, no `SHEP_DOG_NAME`, no
/// `SHEP_NAME`, and a `$SHEP_HOME` that holds no shepherd.
///
/// A build that still polls, or connects, reaches for the socket under that
/// home and exits 5, so the failure is fast instead of a hang.
fn by_hand(args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("a temporary $SHEP_HOME");
    Command::new(env!("CARGO_BIN_EXE_shep-deploy"))
        .args(args)
        .env("SHEP_HOME", home.path())
        .env_remove("SHEP_DOG_NAME")
        .env_remove("SHEP_NAME")
        .output()
        .expect("the test binary's own sibling is on disk")
}

/// Runs the built binary the way shep's own passthrough does: `SHEP_DOG_NAME`
/// set to this dog's registered name, no `SHEP_NAME`, and the given arguments.
///
/// `shep deploy` typed with nothing after it arrives exactly like this. shep's
/// `run_adopted_dog` sets `SHEP_DOG_NAME` and passes no argv, which is the
/// environment the daemon gives the dog it spawns minus the `SHEP_NAME` the
/// daemon adds to every process it starts. That missing half is the only
/// thing that tells the two apart from inside the process.
fn typed_at_a_terminal(args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("a temporary $SHEP_HOME");
    Command::new(env!("CARGO_BIN_EXE_shep-deploy"))
        .args(args)
        .env("SHEP_HOME", home.path())
        .env("SHEP_DOG_NAME", "deploy")
        .env_remove("SHEP_NAME")
        .output()
        .expect("the test binary's own sibling is on disk")
}

/// fails if a bare `shep-deploy` typed by hand starts the poll loop.
///
/// With neither name set the bare command is a person asking what this is, and
/// a second loop beside the adopted dog deploys the same targets twice.
#[test]
fn a_bare_run_by_hand_prints_usage_and_does_not_poll() {
    let output = by_hand(&[]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: shep-deploy"),
        "usage goes to stderr: {:?}",
        output.stderr
    );
    assert!(output.stdout.is_empty(), "nothing is printed to stdout");
}

/// fails if `shep deploy` typed at a terminal starts a second poll loop.
///
/// shep's passthrough sets `SHEP_DOG_NAME` for that command too, so the
/// variable alone reads as the dog contract and the run polls beside the
/// adopted dog: the same targets deployed twice, on a connection that claims
/// to be `deploy` and so gets the real dog restarted when its own handshake is
/// refused. `SHEP_NAME` is what the daemon adds to the dog and this run lacks.
#[test]
fn a_bare_shep_deploy_typed_at_a_terminal_prints_usage_and_does_not_poll() {
    let output = typed_at_a_terminal(&[]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: shep-deploy"),
        "usage goes to stderr: {:?}",
        output.stderr
    );
    assert!(output.stdout.is_empty(), "nothing is printed to stdout");
}

/// fails if help stops printing usage on stdout and exiting 0, in any of its
/// three spellings.
///
/// `--help` used to fall through to a deploy of a sheep named `--help` and
/// fail on a missing `deploy.toml`.
#[test]
fn help_prints_usage_on_stdout_and_exits_zero() {
    for arg in ["-h", "--help", "help"] {
        let output = by_hand(&[arg]);

        assert!(output.status.success(), "{arg}: {:?}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("usage: shep-deploy"),
            "{arg}: usage goes to stdout: {:?}",
            output.stdout
        );
        assert!(output.stderr.is_empty(), "{arg}: nothing on stderr");
    }
}
