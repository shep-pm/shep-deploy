//! Running the release's own build, as the target sheep's user and primary
//! group, and salvaging whatever it produced outside the release.
//!
//! **This is the dangerous step in the whole design.** Every other module
//! here reads a repository or shells out to `git`; this one executes
//! arbitrary code from it. `bun install`'s postinstall scripts are the
//! single most common supply-chain vector in the Node ecosystem, and
//! `make build` is arbitrary by construction - there is no way to make
//! running someone else's build script safe, only ways to bound the damage
//! when it turns out to be malicious. [`run`] is that bound, and how much of
//! a bound it is depends entirely on one field.
//!
//! ## The privilege drop happens only when the app sets `user`
//!
//! **With `user` set**, the build runs as that user's uid, primary gid and
//! nothing else: none of the shepherd's own privilege survives into it. Uid
//! alone would not be enough - the design spec recommends running the
//! shepherd as root specifically so it can drop privileges, and a child that
//! only dropped uid would keep root's own gid for the whole build - see
//! [`run`]'s own doc for the detail.
//!
//! **With `user` unset, which is the default, there is no drop at all.** The
//! build runs as whatever the shepherd runs as. Where the shepherd is root -
//! the arrangement its own docs recommend, precisely so that it CAN drop
//! privileges - a repository's `bun install` postinstall or `make build`
//! executes as root, with the shepherd's full privilege, on every deploy.
//! Nothing here refuses that deploy: this crate's job is to run the build the
//! Flockfile describes, and an app with no `user` is a legitimate
//! configuration that shep itself accepts.
//!
//! That is a deliberate decision by this project's owner rather than an
//! oversight, and it is stated here in full so an operator can act on it:
//! **setting `user` on the app is what turns this module's bound on.**
//! Without it the deploy still works and the failure modes above still
//! hold, a failing build still never reaching the swap, but "a compromised
//! build gets the app's privileges and nothing more" is not one of the
//! guarantees in force. [`run`] warns when it is about to run a build as
//! root with no `user` set; it never refuses one.
//!
//! Three behaviours worth knowing before reading [`run`]'s body:
//!
//! - **An absent build command is a no-op, not an error.** `ReactMap` run
//!   as `bun .` compiles its client with vite's own API at startup and
//!   declares no build at all; the readiness probe already covers that
//!   case, so this module does not need to know a compile happened.
//! - **A failing build is an error that stops everything.** `current`
//!   never moves and the running app is untouched, because it lives in a
//!   directory the build never touches. This is the part that replaces a
//!   hardcoded sleep between a build and a restart.
//! - **`build.artifacts` exists for a build that writes outside the
//!   release.** Warm Rust builds do NOT need it: each sheep gets a
//!   dog-owned cache symlinked in as `target` (see
//!   [`crate::paths::Tree::cache_target`]), so compilation stays warm
//!   across releases AND a hardcoded `./target/release/koji` still
//!   resolves, with nothing copied back. Setting `CARGO_TARGET_DIR`
//!   instead was measured on 2026-08-26 and rejected: with it set
//!   `./target` is never created, so Koji's own `make build`, which ends
//!   in `cp ./target/release/koji koji`, exits 1. `artifacts` remains for
//!   the builds that genuinely do put their output somewhere the release
//!   cannot see, including one an operator points elsewhere themselves.
//!
//! ## Where things live
//!
//! - `artifacts`: copying a declared artifact back into the release.
//! - `contain`: the two roots an artifact may land under, and the check by name.
//! - `env`: the cleared build environment.
//! - `privilege`: resolving `user` to a uid and gid, and the root warning.
//! - `runner`: a prepared command run in its own process group, bounded and killed whole.
//! - `walk`: the handle-based walk both ends of a copy go through.

mod artifacts;
mod contain;
pub(crate) mod env;
pub(crate) mod privilege;
pub(crate) mod runner;
#[cfg(test)]
mod test_support;
mod walk;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::error::Error;
use crate::ownership;
use crate::shared::printable;

use artifacts::copy_artifact;
use contain::artifact_roots;
use privilege::{Step, effective_uid, gid_for, root_warning, uid_for};

/// What to run to turn a freshly checked-out release into something its
/// declared script can execute, and what to salvage from wherever that run
/// actually left its output.
///
/// This is the `[dog.deploy.build]` block of a release's own Flockfile, parsed by
/// [`crate::flockfile::Merged::build_spec`]:
///
/// ```toml
/// [dog.deploy.build]
/// command = "bun install && bun run build"
/// env = { CARGO_TARGET_DIR = "/srv/cache/koji" }
/// artifacts = ["target/release/koji"]
/// ```
///
/// Every field is optional and an absent `[dog.deploy.build]` block is the default
/// value of this struct, which [`run`] treats as a no-op. Unknown keys are
/// refused rather than ignored, matching `shep_core`'s own `AppConfig`: a
/// typo in a build block would otherwise mean a build that silently never
/// runs.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildSpec {
    /// The build command, run through `sh -c` inside the release. `None`
    /// is a no-op, not an error - see [`run`] for why.
    pub command: Option<String>,
    /// Environment for the build, layered over [`env::BASE_ENV`] and the
    /// operator's `passthrough`. The build inherits nothing else from the
    /// process running this dog: see [`run`]. `CARGO_TARGET_DIR` is the one
    /// entry this crate's design gives special meaning to - see
    /// [`artifacts::artifact_source`], and it has to be absolute.
    pub env: BTreeMap<String, String>,
    /// Paths, relative to the release, to copy back in after a successful
    /// build. See the module doc for why this exists at all.
    ///
    /// Refused at parse time if absolute or containing `..` - see
    /// [`contained_artifacts`] for why that refusal is a security boundary
    /// and not a tidiness rule.
    #[serde(deserialize_with = "contained_artifacts")]
    pub artifacts: Vec<PathBuf>,
}

/// Refuses any artifact path that could name something outside the release.
///
/// This runs at parse time, before a build is spawned, because by the time
/// [`copy_artifact`] runs the privilege drop is already behind us: `uid` and
/// `gid` are set on the build's `Command`, so they bound the CHILD, and the
/// copy-back loop runs in this process afterwards at the dog's own uid. Under
/// the arrangement shep's docs recommend, that is root.
///
/// The escape is not hypothetical and the naive case hides it. With no
/// `CARGO_TARGET_DIR`, source and destination are the same expression, so any
/// `..` collapses to `from == to` and [`copy_artifact`]'s self-copy guard
/// returns early. Set `CARGO_TARGET_DIR` and the same string resolves against
/// a different base, so the two differ and the copy proceeds. Measured
/// 2026-08-28: `artifacts = ["target/../../../deploy.toml"]` with
/// `CARGO_TARGET_DIR` set writes through to the tree's own `deploy.toml`,
/// whose `remote` every later fetch reads. A commit on the tracked branch
/// could therefore repoint the deploy at a repository of its own choosing.
///
/// Refused rather than sanitised, matching
/// [`crate::flockfile`]'s treatment of a committed `user`: a path that cannot
/// mean what it says is an operator error worth naming, and silently
/// rewriting one would leave a Flockfile whose text and behaviour disagree.
///
/// # Errors
/// A deserialization error naming the offending path, if any entry is
/// absolute or contains a `..` component.
fn contained_artifacts<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let artifacts = Vec::<PathBuf>::deserialize(deserializer)?;
    for artifact in &artifacts {
        if artifact.as_os_str().is_empty() {
            return Err(D::Error::custom(
                "build.artifacts has an empty entry, which names nothing; remove it or name a \
                 path relative to the release",
            ));
        }
        if artifact.is_absolute() {
            return Err(D::Error::custom(format!(
                "build.artifacts entry `{}` is an absolute path; artifacts are \
                 copied into the release and must be relative to it",
                printable(artifact.display())
            )));
        }
        if artifact
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(D::Error::custom(format!(
                "build.artifacts entry `{}` contains `..`, which would name a \
                 path outside the release",
                printable(artifact.display())
            )));
        }
    }
    Ok(artifacts)
}

/// `Debug` does not print `env`'s values (IR-41).
///
/// `env` is exactly the kind of thing that carries secrets - a registry
/// token for a private package feed is an entirely plausible thing to find
/// in a real `build.env` - so this prints how many variables there are and
/// nothing about what they hold. Mirrors `shep_core`'s own `AppConfig`,
/// which redacts its `env` field the same way for the same reason, rather
/// than inventing a second format for the same problem.
impl fmt::Debug for BuildSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildSpec")
            .field("command", &self.command)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .field("artifacts", &self.artifacts)
            .finish()
    }
}

/// Runs `spec.command` inside `release`, as `as_user` if given, then copies
/// every declared artifact back into the release.
///
/// An absent `command` is a no-op: `spec.artifacts` is never even
/// consulted, so declaring artifacts alongside no command copies nothing
/// rather than failing to find files a build that never ran could not have
/// produced. See the module doc for why an absent command is the right
/// reading rather than a misconfiguration.
///
/// The command runs through `sh -c`, with `release` as its working
/// directory and an environment built from scratch: [`env::BASE_ENV`], then
/// whatever the operator's `passthrough` names, then `spec.env`, and
/// nothing this process itself inherited beyond those. Its stdout and
/// stderr are inherited rather than captured, so
/// an operator watching a deploy sees the build's real output as it
/// happens instead of a blob replayed after the fact.
///
/// ## The one privilege boundary in this whole crate
///
/// When `as_user` is `Some`, the child drops to that user's uid **and**
/// primary gid - resolved via [`uid_for`]/[`gid_for`] - before it runs
/// anything at all. Uid alone is not enough: the design spec recommends
/// running the shepherd as root specifically so it can drop privileges, and
/// a child that only drops uid keeps whatever gid the shepherd itself was
/// running as - root's group, if the shepherd is root - for the entire
/// build. Dropping the gid too closes that gap, and doing both together
/// also clears supplementary groups for free: `std`'s own process-spawn
/// implementation runs `setgroups(0, ...)` whenever a uid and a gid are
/// both set and no explicit group list is given, which is exactly this
/// call shape (verified against the standard library's own source for the
/// unix `Command` spawn path, since this crate calls no `setgroups` or raw
/// libc itself).
///
/// The build executes code from somebody else's repository, so none of the
/// shepherd's own privilege - uid, primary gid, or supplementary groups -
/// may survive into it: a compromised build then gets the target sheep's
/// uid and gid and nothing more, which is the one control in this whole
/// design that actually bounds damage rather than merely detecting it.
///
/// `child.gid(...)` is called before `child.uid(...)` below, matching the
/// general Unix rule that a process must set its group *before* dropping
/// the uid that lets it change groups at all - reversing the two would
/// leave the gid drop silently ineffective once implemented with raw
/// syscalls. For this specific pair of `tokio::process::Command` builder
/// calls that ordering is not actually load-bearing: verified against the
/// standard library's own unix spawn path, `Command::spawn` performs
/// `setgid` before `setuid` internally whenever either is set, regardless
/// of which order the builder methods were called in. The source-level
/// order below is kept anyway because it reads as the intent it has, and
/// because a future change to how this crate drops privilege - raw
/// `libc::setuid`/`setgid`, or a different process API - would need to
/// preserve the real ordering rule this comment states, not just this
/// call's incidental one.
///
/// A different ordering effect *is* real here, and easy to mistake for the
/// syscall one above: `gid_for(...).await` runs before `uid_for(...).await`
/// textually, so for a user that resolves to neither, `gid_for` is the one
/// that fails and its message is the one `run` returns - not because the
/// privilege drop itself works any differently, but because whichever `id`
/// lookup is awaited first is also the one whose error surfaces first. This
/// is why the reject-side test below pins the specific "gid" wording rather
/// than only `Error::Config(_)`, and why `uid_for`/`gid_for` also each have
/// their own direct test - a `run`-level test alone can only ever observe
/// whichever of the two lookups happens to run first.
///
/// A root dog hands `release` and `cache` itself to `as_user` before the
/// build, since the release was made as root, and puts the release's `.git`
/// pointer back afterwards, since owning the directory lets the build replace
/// it. Nothing is handed over when the dog is not root or `as_user` is root.
///
/// ## A failing build stops everything
///
/// A non-zero exit becomes [`Error::Build`]. This is the guard that keeps
/// a broken build from ever reaching the swap: the caller never moves
/// `current`, and the release being built lives in a directory the
/// running app does not share, so nothing already serving traffic is
/// touched. This is the part that replaces a hardcoded sleep between a
/// build and a restart in the deploy scripts this dog exists to retire.
///
/// # Errors
/// [`Error::Config`] if `as_user` names a user `id -u`/`id -g` cannot
/// resolve, if a declared artifact resolves outside the release and the build
/// cache, or if one the build was meant to produce is not there - see
/// [`copy_artifact`] for all three. [`Error::Io`] naming `release` if the shell
/// (or `id`, for `as_user`) cannot even be launched, and naming the artifact's
/// source, its destination, or that destination's parent if a declared
/// artifact cannot be copied - see [`copy_artifact`], which names the source
/// for the copy itself even when it was the destination that failed.
/// [`Error::Config`] also if `as_user` was refused on the way to `release`, see
/// [`ownership::explain_spawn_failure`]. [`Error::Io`] naming the path that
/// failed if a root dog cannot hand the release to `as_user`, see
/// [`ownership::hand_over`].
/// [`Error::Build`] if the command launches and
/// exits non-zero, or is killed by a signal, naming the exit status when there
/// is one. [`Error::BuildTimedOut`] if it runs past `budget`, which is
/// `build_timeout` from the dog's `[deploy]` section.
pub async fn run(
    sheep: &str,
    release: &Path,
    spec: &BuildSpec,
    as_user: Option<&str>,
    passthrough: &[String],
    cache: &Path,
    budget: Duration,
) -> Result<(), Error> {
    let Some(command) = spec.command.as_deref() else {
        return Ok(());
    };

    // Once, here, rather than at parse time or per instance: this is the
    // moment the exposure becomes real, and an absent command never reaches
    // it because there is nothing to run.
    if let Some(warning) = root_warning(sheep, as_user, Step::Build).await {
        eprintln!("{warning}");
    }

    // Refused before the build runs, not when its artifacts are copied: a
    // relative `CARGO_TARGET_DIR` is resolved against wherever this dog
    // happens to be running, so cargo would build somewhere no artifact
    // path could name and the refusal at copy time printed a bare relative
    // path the operator could not find on disk.
    if let Some(target_dir) = spec.env.get("CARGO_TARGET_DIR")
        && !Path::new(target_dir).is_absolute()
    {
        return Err(Error::Config(format!(
            "[dog.deploy.build] env sets CARGO_TARGET_DIR = {target_dir:?}, which is not an \
             absolute path; a relative one is resolved against whatever directory this dog \
             is running in, not against the release. Name the directory in full"
        )));
    }

    let mut child = Command::new("sh");
    child.arg("-c").arg(command);
    child.current_dir(release);

    env::apply(&mut child, sheep, passthrough, &spec.env);

    let mut pointer = None;
    if let Some(user) = as_user {
        let gid = gid_for(release, user).await?;
        let uid = uid_for(release, user).await?;
        if ownership::is_due(effective_uid().await, uid) {
            let (release, cache) = (release.to_owned(), cache.to_owned());
            pointer = crate::shared::off_thread(move || {
                ownership::hand_over(&release, &cache, uid, gid)?;
                ownership::pin_git_pointer(&release, 0)
            })
            .await?;
        }
        child.gid(gid);
        child.uid(uid);
    }

    let built = runner::run_in_group(child, release, budget)
        .await
        .map_err(|error| ownership::explain_spawn_failure(error, as_user, release));

    // However the build ended: a release that fails its build is still
    // reclaimed by retention later, and git must still recognise its pointer.
    if let Some(pointer) = pointer {
        let (path, saved) = (release.to_owned(), pointer);
        let restored =
            crate::shared::off_thread(move || ownership::restore_git_pointer(&path, &saved)).await;
        match (&built, restored) {
            (Ok(()), restored) => restored?,
            (Err(_), Err(error)) => eprintln!(
                "shep-deploy: {sheep}: could not restore the release's .git pointer after the \
                 build: {error}"
            ),
            (Err(_), Ok(())) => {}
        }
    }
    built?;

    // Resolved once for every artifact rather than once per artifact:
    // `canonicalize` walks the whole chain and the two roots do not change.
    // The copies run off the runtime's thread: a release binary can be
    // hundreds of megabytes, and copying it on the one thread stalled the
    // signal handler and the client's reconnect for the duration. See
    // `shared::off_thread`.
    let roots = artifact_roots(release, cache);
    let (release, env, artifacts) = (release.to_owned(), spec.env.clone(), spec.artifacts.clone());
    crate::shared::off_thread(move || {
        for artifact in &artifacts {
            copy_artifact(&release, &roots, &env, artifact)?;
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::test_support::{TEST_BUILD_BUDGET, build};
    use super::*;
    use crate::fixtures;
    use std::fs;

    /// fails if an absent build command is an error rather than a no-op.
    /// ReactMap run as `bun .` compiles its client at startup and declares
    /// no build at all; the readiness probe covers it.
    #[tokio::test]
    async fn an_absent_build_command_is_not_an_error() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec::default();
        assert!(build(rel.path(), &spec).await.is_ok());
    }

    /// fails if declared artifacts are not copied into the release. With
    /// CARGO_TARGET_DIR pointed at a shared cache, the binary lands outside
    /// the release, and `script = ./target/release/koji` would resolve to
    /// nothing.
    #[tokio::test]
    async fn declared_artifacts_are_copied_into_the_release() {
        let rel = fixtures::fixture_release(&[]);
        // The redirect points at the dog's OWN cache, which is the only
        // outside-the-release source that is still allowed. See
        // `a_source_outside_the_release_and_cache_is_refused` for why.
        let cache = fixtures::tempdir();
        std::fs::create_dir_all(cache.path().join("release")).unwrap();
        std::fs::write(cache.path().join("release/koji"), b"binary").unwrap();
        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/koji")],
        };
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");
        assert!(rel.path().join("target/release/koji").exists());
    }

    /// fails if an escaping artifact reaches a build at all.
    ///
    /// The refusal above is the last line; this is the first. A Flockfile
    /// naming such a path is refused when it is parsed, so no build is ever
    /// spawned for it, matching how `crate::flockfile` refuses a committed
    /// `user` rather than ignoring it.
    #[test]
    fn an_escaping_artifact_is_refused_at_parse_time() {
        for bad in ["target/../../../deploy.toml", "/etc/passwd"] {
            let toml = format!("command = \"true\"\nartifacts = [\"{bad}\"]\n");
            let err = toml::from_str::<BuildSpec>(&toml)
                .expect_err("an escaping artifact must not parse");
            assert!(
                format!("{err}").contains(bad),
                "the refusal must name the entry, got: {err}"
            );
        }
    }

    /// fails if `Debug` stops redacting `env`'s values, or starts hiding a
    /// field entirely instead of just its values. Exact string pinned so a
    /// lazy `derive(Debug)` refactor fails here, the same way shep_core's
    /// own `AppConfig` test is pinned.
    #[test]
    fn debug_redacts_env_values() {
        let spec = BuildSpec {
            command: Some("make build".into()),
            env: [
                ("REGISTRY_TOKEN".to_string(), "secret".to_string()),
                ("RUST_LOG".to_string(), "info".to_string()),
            ]
            .into(),
            artifacts: vec![],
        };
        assert_eq!(
            format!("{spec:?}"),
            "BuildSpec { command: Some(\"make build\"), env: <2 vars>, artifacts: [] }"
        );
    }

    /// fails if an absent build command stops being a no-op the moment
    /// artifacts are also declared. `run` must return before ever looking
    /// at `spec.artifacts` when there is no command - a build that never
    /// ran cannot have produced anything, so attempting the copy here would
    /// fail on a file that was never going to exist, contradicting "an
    /// absent build command is a no-op".
    ///
    /// `CARGO_TARGET_DIR` is set here specifically so the declared
    /// artifact's source and destination are genuinely different paths -
    /// with no override, both sides of `copy_artifact` collapse to the
    /// same `release`-relative path and its self-copy guard would return
    /// `Ok` for a reason that has nothing to do with the command being
    /// absent, letting this test pass even if the early return above it
    /// were deleted entirely.
    #[tokio::test]
    async fn an_absent_command_skips_artifacts_too() {
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let spec = BuildSpec {
            command: None,
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/nothing-built-this")],
        };
        assert!(build(rel.path(), &spec).await.is_ok());
    }

    /// fails if the self-copy guard in `copy_artifact` is removed. Without
    /// it, declaring an artifact that the build already placed directly
    /// inside the release (the ordinary case with no `CARGO_TARGET_DIR`
    /// redirect) would copy the file onto itself - verified empirically to
    /// truncate it to zero bytes, since `fs::copy` opens its destination
    /// for writing before it reads the source. This test would go red
    /// immediately if that guard were deleted, since the content would come
    /// back empty rather than what the build actually wrote.
    #[tokio::test]
    async fn an_artifact_already_in_the_release_is_left_intact() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("mkdir -p dist && printf hello > dist/app.js".into()),
            artifacts: vec![PathBuf::from("dist/app.js")],
            ..Default::default()
        };
        build(rel.path(), &spec).await.expect("builds");
        let contents = fs::read_to_string(rel.path().join("dist/app.js")).expect("reads");
        assert_eq!(contents, "hello");
    }
}
