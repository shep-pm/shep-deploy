//! The release and after hooks: commands the operator names in the sheep's
//! `[app.dogs.deploy]` table. The release hook runs once per deploy after the
//! build and before `current` is swapped; the after hook runs once the new
//! release is swapped in, verified and recorded.
//!
//! It exists for what a build is the wrong place for. A database migration
//! ends the build command otherwise, so its password reaches every script the
//! build runs and its output sits unlabelled in the build log. The hook has
//! its own `env`, its own line in the log and its own failure.
//!
//! ```toml
//! [app.dogs.deploy.release]
//! command = "supabase db push"
//! env = { SUPABASE_DB_PASSWORD = "..." }
//! ```
//!
//! ```toml
//! [app.dogs.deploy.after]
//! command = "purge-cache"
//! env = { CLOUDFLARE_API_TOKEN = "..." }
//! ```
//!
//! The tables are the operator's alone: they are read from the shepherd's
//! record of the sheep, never from a release. See [`crate::settings`]. A failing
//! release hook stops the deploy before the swap exactly as a failing build
//! does. A failing after hook changes nothing: the release is already live.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::build::env;
use crate::build::privilege::{Step, root_warning};
use crate::build::runner;
use crate::build::{drop_to, settle_pointer};
use crate::error::Error;

/// A hook's command and the environment it alone receives.
///
/// Every field is optional and an absent table is the default value of this
/// struct, which [`run`] treats as a no-op. Unknown keys are refused, as in
/// [`crate::build::BuildSpec`], so a typo does not become a hook that never
/// runs.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookSpec {
    /// The command, run through `sh -c` inside the release. `None` is a
    /// no-op.
    pub command: Option<String>,
    /// Environment for the hook, layered over `BASE_ENV` and the dog's
    /// `passthrough`. The dog's own three variables are set after it and win.
    pub env: BTreeMap<String, String>,
}

/// `Debug` does not print `env`'s values (IR-41): a hook's `env` is where the
/// operator puts the database password.
impl fmt::Debug for HookSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookSpec")
            .field("command", &self.command)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish()
    }
}

/// Every hook a release's Flockfile declares.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hooks {
    /// Runs after the build and before the swap: `[app.dogs.deploy.release]`.
    pub release: HookSpec,
    /// Runs once the new release is swapped in, verified and recorded:
    /// `[app.dogs.deploy.after]`.
    pub after: HookSpec,
}

/// How a hook that ran came to an end without succeeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It exited with this status, or `None` if a signal killed it.
    Exited(Option<i32>),
    /// It ran past this budget and was abandoned, process group and all.
    TimedOut(Duration),
}

/// What a hook run needs to know about the deploy it belongs to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Context<'a> {
    /// The sheep being deployed.
    pub sheep: &'a str,
    /// The sha being deployed.
    pub sha: &'a str,
    /// The release directory the hook runs in.
    pub release: &'a Path,
    /// The app's `user`, which the hook drops to as the build does.
    pub as_user: Option<&'a str>,
    /// The dog's `passthrough`.
    pub passthrough: &'a [String],
    /// The dog's `build_timeout`.
    pub budget: Duration,
    /// The build cache directory, handed over with the release.
    pub cache: &'a Path,
    /// Whether a build command ran in the release, which handed it over to
    /// `as_user` already.
    pub built: bool,
}

/// The name of the release hook in [`Error::Hook`].
const RELEASE: &str = "release";

/// The name of the after hook in [`Error::Hook`].
const AFTER: &str = "after";

/// Runs the release hook `spec`. See [`run_named`].
///
/// # Errors
/// As [`run_named`], with `name` `"release"`.
pub(crate) async fn run(spec: &HookSpec, ctx: &Context<'_>) -> Result<(), Error> {
    run_named(RELEASE, Step::ReleaseHook, spec, ctx).await
}

/// Runs the after hook `spec`. See [`run_named`].
///
/// # Errors
/// As [`run_named`], with `name` `"after"`.
pub(crate) async fn run_after(spec: &HookSpec, ctx: &Context<'_>) -> Result<(), Error> {
    run_named(AFTER, Step::AfterHook, spec, ctx).await
}

/// Runs the hook `spec`, called `name` in the log and in [`Error::Hook`],
/// inside `ctx.release`, as `ctx.as_user` if given, bounded by `ctx.budget`.
///
/// An absent command is a no-op. Otherwise this prints one line to stderr
/// naming the sheep and the step, so the migration is its own step in the
/// log, and runs the command through the build's group runner: `sh -c` in a
/// process group of its own, abandoned whole past the budget and killed when
/// it ends.
///
/// The environment is cleared and rebuilt as the build's is: `BASE_ENV`, the
/// dog's `passthrough`, then `spec.env`. The dog's own `SHEP_DEPLOY_SHEEP`,
/// `SHEP_DEPLOY_SHA` and `SHEP_DEPLOY_RELEASE` (an absolute path) go on last,
/// so a hook's `env` cannot override them.
///
/// A root dog hands the release and the cache to `as_user` first when no build
/// ran to do it, as the build does, so a hook in an app with no build command
/// can write in its release. Either way it pins the release's `.git` pointer
/// and writes it back after the hook, since the hook owns the directory and
/// runs code the build put there.
///
/// # Errors
/// [`Error::Config`] if `ctx.as_user` names a user `id` cannot resolve.
/// [`Error::Io`] naming the release if the shell cannot be launched, or the
/// path that failed if a root dog cannot hand the release to `as_user`.
/// [`Error::Hook`] if the command exits non-zero, is killed by a signal, or
/// runs past `ctx.budget`.
async fn run_named(
    name: &'static str,
    step: Step,
    spec: &HookSpec,
    ctx: &Context<'_>,
) -> Result<(), Error> {
    let Some(command) = spec.command.as_deref() else {
        return Ok(());
    };

    if let Some(warning) = root_warning(ctx.sheep, ctx.as_user, step).await {
        eprintln!("{warning}");
    }
    eprintln!("shep-deploy: {}: running the {name} hook", ctx.sheep);

    let release = std::path::absolute(ctx.release).map_err(Error::at(ctx.release))?;
    let mut child = Command::new("sh");
    child.arg("-c").arg(command).current_dir(&release);
    env::apply(&mut child, ctx.sheep, ctx.passthrough, &spec.env);
    child
        .env("SHEP_DEPLOY_SHEEP", ctx.sheep)
        .env("SHEP_DEPLOY_SHA", ctx.sha)
        .env("SHEP_DEPLOY_RELEASE", &release);

    let mut pointer = None;
    if let Some(user) = ctx.as_user {
        let dropped = drop_to(ctx.release, ctx.cache, user, !ctx.built).await?;
        pointer = dropped.pointer;
        let (uid, gid) = (dropped.uid, dropped.gid);
        child.gid(gid);
        child.uid(uid);
    }

    let ran = runner::run_in_group(child, ctx.release, ctx.budget).await;
    settle_pointer(
        ctx.sheep,
        ctx.release,
        pointer,
        &ran,
        &format!("the {name} hook"),
    )
    .await?;
    ran.map_err(|error| match error {
        Error::Build { status } => Error::Hook {
            name,
            ending: Ending::Exited(status),
        },
        Error::BuildTimedOut { after } => Error::Hook {
            name,
            ending: Ending::TimedOut(after),
        },
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BUDGET: Duration = Duration::from_secs(60);

    async fn run_hook(
        rel: &Path,
        spec: &HookSpec,
        passthrough: &[String],
        budget: Duration,
    ) -> Result<(), Error> {
        let ctx = Context {
            sheep: "web",
            sha: crate::fixtures::SHA,
            release: rel,
            as_user: None,
            passthrough,
            budget,
            cache: &rel.join("cache"),
            built: true,
        };
        run(spec, &ctx).await
    }

    fn hook(command: &str, env: &[(&str, &str)]) -> HookSpec {
        HookSpec {
            command: Some(command.to_owned()),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn written(rel: &Path, name: &str) -> String {
        std::fs::read_to_string(rel.join(name))
            .unwrap_or_else(|e| panic!("the hook never wrote {name}: {e}"))
    }

    /// fails if the hook runs anywhere but the release, or inherits the dog's
    /// environment. `CARGO_PKG_NAME` is set for this test process and is in
    /// neither `BASE_ENV` nor the hook's `passthrough`.
    #[tokio::test]
    async fn a_hook_runs_in_the_release_with_a_cleared_environment() {
        assert!(
            std::env::var("CARGO_PKG_NAME").is_ok(),
            "the probe variable must exist in this process or the test proves nothing"
        );
        let rel = crate::fixtures::fixture_release(&[]);
        let spec = hook(
            "pwd -P > cwd.txt; printenv CARGO_PKG_NAME > leaked.txt; \
             printenv DB_PASSWORD > own.txt; printenv PATH > path.txt; true",
            &[("DB_PASSWORD", "hunter2")],
        );

        run_hook(rel.path(), &spec, &[], TEST_BUDGET)
            .await
            .expect("hook runs");

        let cwd = written(rel.path(), "cwd.txt");
        assert_eq!(
            Path::new(cwd.trim()),
            rel.path().canonicalize().expect("canonical")
        );
        assert!(written(rel.path(), "leaked.txt").trim().is_empty());
        assert_eq!(written(rel.path(), "own.txt").trim(), "hunter2");
        assert!(!written(rel.path(), "path.txt").trim().is_empty());
    }

    /// fails if a hook's own `env` can override the variables the dog sets,
    /// which a hook script may rely on to know what it is deploying.
    #[tokio::test]
    async fn the_dogs_variables_win_over_the_hooks_env() {
        let rel = crate::fixtures::fixture_release(&[]);
        let spec = hook(
            "printenv SHEP_DEPLOY_SHEEP > sheep.txt; printenv SHEP_DEPLOY_SHA > sha.txt; \
             printenv SHEP_DEPLOY_RELEASE > release.txt",
            &[
                ("SHEP_DEPLOY_SHEEP", "other"),
                ("SHEP_DEPLOY_SHA", "other"),
                ("SHEP_DEPLOY_RELEASE", "other"),
            ],
        );

        run_hook(rel.path(), &spec, &[], TEST_BUDGET)
            .await
            .expect("hook runs");

        assert_eq!(written(rel.path(), "sheep.txt").trim(), "web");
        assert_eq!(written(rel.path(), "sha.txt").trim(), crate::fixtures::SHA);
        let release = written(rel.path(), "release.txt");
        assert!(Path::new(release.trim()).is_absolute(), "{release}");
        assert_eq!(
            Path::new(release.trim()).canonicalize().expect("exists"),
            rel.path().canonicalize().expect("canonical")
        );
    }

    /// fails if a variable named in `passthrough` stops reaching the hook.
    #[tokio::test]
    async fn a_named_passthrough_variable_reaches_the_hook() {
        let rel = crate::fixtures::fixture_release(&[]);
        let spec = hook("printenv CARGO_PKG_NAME > passed.txt", &[]);

        run_hook(
            rel.path(),
            &spec,
            &["CARGO_PKG_NAME".to_owned()],
            TEST_BUDGET,
        )
        .await
        .expect("hook runs");

        assert_eq!(written(rel.path(), "passed.txt").trim(), "shep-deploy");
    }

    /// fails if an absent command stops being a no-op.
    #[tokio::test]
    async fn an_absent_command_runs_nothing() {
        let rel = crate::fixtures::fixture_release(&[]);

        run_hook(rel.path(), &HookSpec::default(), &[], TEST_BUDGET)
            .await
            .expect("a no-op");
    }

    /// fails if a hook that exits non-zero is treated as success, which would
    /// let a failed migration reach the swap.
    #[tokio::test]
    async fn a_hook_that_exits_non_zero_fails() {
        let rel = crate::fixtures::fixture_release(&[]);

        let err = run_hook(rel.path(), &hook("exit 3", &[]), &[], TEST_BUDGET)
            .await
            .expect_err("a failing hook");

        assert!(
            matches!(
                err,
                Error::Hook {
                    name: "release",
                    ending: Ending::Exited(Some(3))
                }
            ),
            "{err:?}"
        );
    }

    /// fails if a hook that never finishes is waited on forever, or if
    /// abandoning it leaves what it started running.
    ///
    /// Not `start_paused`: see the note on the timeout in `build::runner`.
    #[tokio::test]
    async fn a_hook_that_never_finishes_is_abandoned() {
        let rel = crate::fixtures::fixture_release(&[]);
        // A backgrounded descendant keeps the shell alive, so the test is
        // about the group kill and not about `child.kill()`.
        let spec = hook("sleep 600 & echo $! > descendant.pid; wait", &[]);

        let err = run_hook(rel.path(), &spec, &[], Duration::from_secs(2))
            .await
            .expect_err("must be abandoned");

        assert!(
            matches!(
                err,
                Error::Hook {
                    ending: Ending::TimedOut(_),
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("build_timeout"), "{err}");
        let pid = written(rel.path(), "descendant.pid");
        assert!(
            !still_running(pid.trim()),
            "the descendant (pid {}) outlived the hook",
            pid.trim()
        );
    }

    /// Whether `pid` is alive, polled because a signalled group is not reaped
    /// in the same instant. `kill -0` because the crate forbids unsafe.
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

    /// fails if `Debug` prints a hook's env values.
    #[test]
    fn debug_redacts_env_values() {
        let spec = HookSpec {
            command: Some("supabase db push".into()),
            env: [("SUPABASE_DB_PASSWORD".to_owned(), "hunter2".to_owned())].into(),
        };

        assert_eq!(
            format!("{spec:?}"),
            "HookSpec { command: Some(\"supabase db push\"), env: <1 vars> }"
        );
    }

    /// fails if a hook, with or without a build before it, cannot run as the
    /// dog's own user: the hand-over and the pointer pin must be no-ops there,
    /// not errors. Handing a release to another user needs root, which
    /// `ownership`'s own tests cover.
    #[tokio::test]
    async fn a_hook_runs_as_the_dogs_own_user_after_a_build_or_without_one() {
        for built in [false, true] {
            let rel = crate::fixtures::fixture_release(&[]);
            let me = tokio::process::Command::new("id")
                .arg("-un")
                .output()
                .await
                .expect("id runs");
            let me = String::from_utf8(me.stdout)
                .expect("utf8")
                .trim()
                .to_owned();
            let cache = rel.path().join("cache");
            let ctx = Context {
                sheep: "web",
                sha: crate::fixtures::SHA,
                release: rel.path(),
                as_user: Some(&me),
                passthrough: &[],
                budget: TEST_BUDGET,
                cache: &cache,
                built,
            };
            run(&hook("touch ran", &[]), &ctx).await.expect("runs");
            assert!(rel.path().join("ran").exists());
        }
    }
}
