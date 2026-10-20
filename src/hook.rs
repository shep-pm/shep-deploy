//! The release hook: a command the operator names in their own
//! `Flockfile.override.toml`, run once per deploy after the build and before
//! `current` is swapped.
//!
//! It exists for what a build is the wrong place for. A database migration
//! ends the build command otherwise, so its password reaches every script the
//! build runs and its output sits unlabelled in the build log. The hook has
//! its own `env`, its own line in the log and its own failure.
//!
//! ```toml
//! [dog.deploy.release]
//! command = "supabase db push"
//! env = { SUPABASE_DB_PASSWORD = "..." }
//! ```
//!
//! The table is the operator's alone, see [`refuse_repo_hooks`]. A failing
//! hook stops the deploy before the swap exactly as a failing build does.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use toml::Value;

use crate::build::env;
use crate::build::privilege::{Step, gid_for, root_warning, uid_for};
use crate::build::runner;
use crate::error::Error;
use crate::shared::printable;

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
    /// Runs after the build and before the swap: `[dog.deploy.release]`.
    pub release: HookSpec,
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
}

/// The name of the release hook in [`Error::Hook`].
const RELEASE: &str = "release";

/// Runs the release hook `spec` inside `ctx.release`, as `ctx.as_user` if
/// given, bounded by `ctx.budget`.
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
/// Nothing is handed over to `as_user` here: the build's run already did that,
/// and a hook is the operator's command, not the repository's.
///
/// # Errors
/// [`Error::Config`] if `ctx.as_user` names a user `id` cannot resolve.
/// [`Error::Io`] naming the release if the shell cannot be launched.
/// [`Error::Hook`] if the command exits non-zero, is killed by a signal, or
/// runs past `ctx.budget`.
pub(crate) async fn run(spec: &HookSpec, ctx: &Context<'_>) -> Result<(), Error> {
    let Some(command) = spec.command.as_deref() else {
        return Ok(());
    };

    if let Some(warning) = root_warning(ctx.sheep, ctx.as_user, Step::ReleaseHook).await {
        eprintln!("{warning}");
    }
    eprintln!("shep-deploy: {}: running the {RELEASE} hook", ctx.sheep);

    let release = std::path::absolute(ctx.release).map_err(Error::at(ctx.release))?;
    let mut child = Command::new("sh");
    child.arg("-c").arg(command).current_dir(&release);
    env::apply(&mut child, ctx.sheep, ctx.passthrough, &spec.env);
    child
        .env("SHEP_DEPLOY_SHEEP", ctx.sheep)
        .env("SHEP_DEPLOY_SHA", ctx.sha)
        .env("SHEP_DEPLOY_RELEASE", &release);

    if let Some(user) = ctx.as_user {
        let gid = gid_for(ctx.release, user).await?;
        let uid = uid_for(ctx.release, user).await?;
        child.gid(gid);
        child.uid(uid);
    }

    runner::run_in_group(child, ctx.release, ctx.budget)
        .await
        .map_err(|error| match error {
            Error::Build { status } => Error::Hook {
                name: RELEASE,
                ending: Ending::Exited(status),
            },
            Error::BuildTimedOut { after } => Error::Hook {
                name: RELEASE,
                ending: Ending::TimedOut(after),
            },
            other => other,
        })
}

/// The tables under `[dog.deploy]` that carry an operator's command.
///
/// `after` has no reader yet; it is refused here so the ticket that adds one
/// needs no second change to the refusal.
const OPERATOR_TABLES: [&str; 2] = ["release", "after"];

/// The table `name` under `[dog.deploy]` in `doc`, whatever its shape.
fn deploy_table<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    doc.as_table()?
        .get("dog")?
        .as_table()?
        .get("deploy")?
        .as_table()?
        .get(name)
}

/// Refuses a document the repository could have written that carries a hook.
///
/// `file` names the document for the message. Called on the committed
/// `Flockfile.toml` always, and on `Flockfile.override.toml` unless the dog
/// linked it in from the operator's checkout: a hook's `env` holds the
/// operator's secrets, and a commit must not choose the command that receives
/// them.
///
/// # Errors
/// [`Error::Config`] naming `file` and the table, for the first of
/// `[dog.deploy.release]` and `[dog.deploy.after]` that `doc` has.
pub(crate) fn refuse_repo_hooks(doc: &Value, file: &str) -> Result<(), Error> {
    for name in OPERATOR_TABLES {
        if deploy_table(doc, name).is_some() {
            return Err(Error::Config(format!(
                "{file} sets `[dog.deploy.{name}]`: a hook's env holds the operator's secrets, \
                 and a commit must not choose the command that receives them. Set it in the \
                 Flockfile.override.toml in the operator's checkout instead - it is gitignored \
                 and never comes from the repo."
            )));
        }
    }
    Ok(())
}

/// The hooks in the merged document `doc`, read from `release`'s Flockfiles.
///
/// # Errors
/// [`Error::Config`] naming `release` if `[dog.deploy.release]` does not match
/// [`HookSpec`]'s schema: an unknown key, or a value of the wrong type.
pub(crate) fn parse(doc: &Value, release: &Path) -> Result<Hooks, Error> {
    let Some(table) = deploy_table(doc, "release") else {
        return Ok(Hooks::default());
    };
    let release_hook = table
        .clone()
        .try_into()
        .map_err(|source: toml::de::Error| {
            Error::Config(format!(
                "{}: `[dog.deploy.release]` does not match the hook schema: {}",
                release.display(),
                printable(source.message())
            ))
        })?;
    Ok(Hooks {
        release: release_hook,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::flockfile::{self, OVERRIDE};
    use crate::shared::FromCheckout;

    const APP: &str = "[[app]]\nname = \"web\"\nscript = \"./run\"\n";
    const RELEASE_HOOK: &str = "[dog.deploy.release]\ncommand = \"true\"\n";
    const AFTER_HOOK: &str = "[dog.deploy.after]\ncommand = \"true\"\n";

    fn release_with(committed: &str, committed_override: Option<&str>) -> tempfile::TempDir {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(rel.path().join("Flockfile.toml"), committed).expect("committed");
        if let Some(text) = committed_override {
            std::fs::write(rel.path().join(OVERRIDE), text).expect("override");
        }
        rel
    }

    /// fails if a commit can choose the command that receives the operator's
    /// secrets.
    #[test]
    fn a_committed_release_hook_is_refused() {
        let rel = release_with(&format!("{APP}{RELEASE_HOOK}"), None);

        let err = flockfile::read(rel.path(), &FromCheckout::of(vec![])).expect_err("refused");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("Flockfile.toml"), "{shown}");
        assert!(shown.contains("[dog.deploy.release]"), "{shown}");
    }

    /// fails if the sibling table is left open for the ticket that reads it.
    #[test]
    fn a_committed_after_hook_is_refused() {
        let rel = release_with(&format!("{APP}{AFTER_HOOK}"), None);

        let err = flockfile::read(rel.path(), &FromCheckout::of(vec![])).expect_err("refused");

        let shown = err.to_string();
        assert!(shown.contains("Flockfile.toml"), "{shown}");
        assert!(shown.contains("[dog.deploy.after]"), "{shown}");
    }

    /// fails if a repository can supply its own `Flockfile.override.toml` and
    /// have a hook honoured: only a file the dog linked in from the operator's
    /// checkout is theirs.
    #[test]
    fn an_override_the_dog_did_not_link_is_refused_a_hook() {
        let rel = release_with(APP, Some(RELEASE_HOOK));

        let err = flockfile::read(rel.path(), &FromCheckout::of(vec![])).expect_err("refused");

        let shown = err.to_string();
        assert!(shown.contains(OVERRIDE), "{shown}");
        assert!(shown.contains("[dog.deploy.release]"), "{shown}");
    }

    /// fails if the operator loses the one place a hook may be set.
    #[test]
    fn the_operators_override_may_set_a_release_hook() {
        let rel = release_with(APP, None);
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            checkout.path().join(OVERRIDE),
            "[dog.deploy.release]\ncommand = \"supabase db push\"\n\
             env = { SUPABASE_DB_PASSWORD = \"hunter2\" }\n",
        )
        .expect("the operator's override");
        std::os::unix::fs::symlink(checkout.path().join(OVERRIDE), rel.path().join(OVERRIDE))
            .expect("linked in");

        let merged = flockfile::read(rel.path(), &FromCheckout::of(vec![PathBuf::from(OVERRIDE)]))
            .expect("the operator may set a hook");
        let hooks = merged.hooks().expect("parses");

        assert_eq!(hooks.release.command.as_deref(), Some("supabase db push"));
        assert_eq!(
            hooks
                .release
                .env
                .get("SUPABASE_DB_PASSWORD")
                .map(String::as_str),
            Some("hunter2")
        );
    }

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

    /// fails if a typo in a hook's key becomes a hook that silently never
    /// runs.
    #[test]
    fn an_unknown_hook_key_is_refused() {
        let doc: Value = "[dog.deploy.release]\ncommnd = \"true\"\n"
            .parse()
            .expect("toml");

        let err = parse(&doc, Path::new("/srv/web/releases/abc")).expect_err("unknown key");

        let shown = err.to_string();
        assert!(shown.contains("commnd"), "{shown}");
        assert!(shown.contains("[dog.deploy.release]"), "{shown}");
    }

    /// fails if a Flockfile without a hook is anything but the no-op.
    #[test]
    fn an_absent_hook_block_is_the_default() {
        let doc: Value = APP.parse().expect("toml");

        let hooks = parse(&doc, Path::new("/srv/web/releases/abc")).expect("parses");

        assert_eq!(hooks, Hooks::default());
        assert_eq!(hooks.release.command, None);
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
}
