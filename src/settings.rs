//! What the operator told shep about how to deploy one sheep: its
//! `[app.dogs.deploy]` table.
//!
//! The table lives in the shepherd's own record of the sheep, not in the
//! release. A release's Flockfile is whatever the tracked branch carried at its
//! sha, so a setting read from it is chosen by whoever can land a commit; the
//! build command above all, and a hook's `env` holds the operator's secrets.
//! A sheep also gets its own table, where one Flockfile used to cover every
//! sheep built from the same repository.
//!
//! ```toml
//! [app.dogs.deploy]
//! command = "bun install && bun run build"
//! artifacts = ["target/release/koji"]
//! reload = ["cmd/voting", "assets/*.png"]
//!
//! [app.dogs.deploy.release]
//! command = "supabase db push"
//! env = { SUPABASE_DB_PASSWORD = "..." }
//!
//! [app.dogs.deploy.after]
//! command = "purge-cache"
//! ```
//!
//! No table at all is a sheep with no build, no hooks and no reload paths.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use shep_client::shep_core::config::AppConfig;

use crate::build::{BuildSpec, contained_artifacts};
use crate::daemon::Daemon;
use crate::error::Error;
use crate::hook::{HookSpec, Hooks};
use crate::shared::printable;

/// The name this dog's table goes by under a sheep's `dogs`.
const TABLE: &str = "deploy";

/// One sheep's `[app.dogs.deploy]` table.
///
/// Unknown keys are refused rather than ignored, as in [`BuildSpec`]: a typo
/// in `reload` would otherwise be a filter that is never applied, and one in
/// `command` a build that never runs, each with nothing saying so.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The build command, run through `sh -c` inside the release. `None` is
    /// no build.
    pub command: Option<String>,
    /// Paths, relative to the release, to copy back in after a successful
    /// build. See [`BuildSpec::artifacts`].
    #[serde(deserialize_with = "contained_artifacts")]
    pub artifacts: Vec<PathBuf>,
    /// Git pathspecs, relative to the repository root. A directory name
    /// matches its contents. Empty means every new sha deploys.
    pub reload: Vec<String>,
    /// Runs after the build and before the swap.
    pub release: HookSpec,
    /// Runs once the new release is swapped in, verified and recorded.
    pub after: HookSpec,
}

impl Settings {
    /// The build these settings describe.
    pub(crate) fn build_spec(&self) -> BuildSpec {
        BuildSpec {
            command: self.command.clone(),
            artifacts: self.artifacts.clone(),
            ..BuildSpec::default()
        }
    }

    /// The hooks these settings describe.
    pub(crate) fn hooks(&self) -> Hooks {
        Hooks {
            release: self.release.clone(),
            after: self.after.clone(),
        }
    }
}

/// `sheep`'s settings as the shepherd holds them.
///
/// # Errors
/// Whatever [`Daemon::sheep_config`] returns, and [`Error::Config`] as
/// [`from_app`].
pub(crate) async fn read<D: Daemon>(daemon: &D, sheep: &str) -> Result<Settings, Error> {
    from_app(sheep, &daemon.sheep_config(sheep).await?)
}

/// The settings in `app`'s `[app.dogs.deploy]`, or none if it has no such
/// table.
///
/// # Errors
/// [`Error::Config`] naming `sheep` if the table does not match
/// [`Settings`]: an unknown key, or a value of the wrong type.
pub(crate) fn from_app(sheep: &str, app: &AppConfig) -> Result<Settings, Error> {
    let Some(table) = app.dogs.get(TABLE) else {
        return Ok(Settings::default());
    };
    serde_json::from_value(Value::Object(table.as_map().clone())).map_err(|source| {
        Error::Config(format!(
            "{sheep}: `[app.dogs.deploy]` does not match the settings schema: {}",
            printable(without_value(&source.to_string()))
        ))
    })
}

/// `message` with the offending value cut out of a type or value error.
///
/// serde quotes what it could not use (`invalid type: string "hunter2",
/// expected a map`), and a hook's `env` is where the operator keeps secrets.
/// An unknown key is kept: naming it is the point of refusing it.
fn without_value(message: &str) -> String {
    for kind in ["invalid type", "invalid value"] {
        if let Some(rest) = message.strip_prefix(kind)
            && rest.starts_with(": ")
            && let Some(at) = rest.rfind(", expected ")
        {
            return format!("{kind}{}", &rest[at..]);
        }
    }
    message.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    /// fails if a table does not reach every key this dog reads, or if
    /// `env`, which only a hook carries, is lost on the way.
    #[test]
    fn a_table_parses_into_settings() {
        let app = fixtures::app_with_settings(
            "web",
            "command = 'make build'\nartifacts = ['target/release/koji']\n\
             reload = ['cmd/voting']\n\
             [release]\ncommand = 'migrate'\nenv = { PASSWORD = 'hunter2' }\n\
             [after]\ncommand = 'purge-cache'\n",
        );

        let settings = from_app("web", &app).expect("parses");

        assert_eq!(settings.command.as_deref(), Some("make build"));
        assert_eq!(settings.artifacts, [PathBuf::from("target/release/koji")]);
        assert_eq!(settings.reload, ["cmd/voting"]);
        assert_eq!(settings.release.command.as_deref(), Some("migrate"));
        assert_eq!(
            settings.release.env.get("PASSWORD").map(String::as_str),
            Some("hunter2")
        );
        assert_eq!(settings.after.command.as_deref(), Some("purge-cache"));
    }

    /// fails if a sheep with no table, or an empty one, stops meaning "no
    /// build, no hooks, every sha deploys".
    #[test]
    fn no_table_and_an_empty_table_are_both_the_default() {
        let bare = fixtures::app_named("web");
        assert_eq!(from_app("web", &bare).expect("parses"), Settings::default());

        let empty = fixtures::app_with_settings("web", "");
        assert_eq!(
            from_app("web", &empty).expect("parses"),
            Settings::default()
        );
    }

    /// fails if a typo is ignored, which is a build or a filter that is
    /// never applied with nothing saying so. The sheep is named because
    /// the table is not in any file the operator has open at that moment.
    #[test]
    fn an_unknown_key_is_refused_naming_the_key_and_the_sheep() {
        let app = fixtures::app_with_settings("web", "commnd = 'make'\n");

        let err = from_app("web", &app).expect_err("refused");

        assert!(matches!(err, Error::Config(_)), "{err}");
        let shown = err.to_string();
        assert!(shown.contains("commnd"), "{shown}");
        assert!(shown.contains("web"), "{shown}");
    }

    /// fails if a hook's key is checked less strictly than the table's.
    #[test]
    fn an_unknown_hook_key_is_refused() {
        let app = fixtures::app_with_settings("web", "[after]\ncommnd = 'true'\n");

        let err = from_app("web", &app).expect_err("refused");

        assert!(err.to_string().contains("commnd"), "{err}");
    }

    /// fails if an artifact that can leave the release is accepted because it
    /// came from the operator's table: the copy-back runs at the dog's own
    /// uid whoever wrote the path.
    #[test]
    fn an_artifact_outside_the_release_is_refused() {
        let app = fixtures::app_with_settings("web", "artifacts = ['../../etc/passwd']\n");

        let err = from_app("web", &app).expect_err("refused");

        assert!(err.to_string().contains(".."), "{err}");
    }

    /// fails if a value of the wrong type is echoed into the error: a hook's
    /// `env` holds secrets, and serde quotes what it could not use.
    #[test]
    fn a_wrong_typed_value_is_not_echoed() {
        for table in [
            "[release]\nenv = { PASSWORD = 1234567 }\n",
            "[release]\nenv = 'hunter2'\n",
            "release = 'hunter2'\n",
            "reload = 'hunter2'\n",
        ] {
            let app = fixtures::app_with_settings("web", table);

            let shown = from_app("web", &app).expect_err("refused").to_string();

            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(!shown.contains("1234567"), "{shown}");
            assert!(shown.contains("web"), "{shown}");
        }
    }

    /// fails if `Debug` prints a hook's `env` values through `Settings`.
    #[test]
    fn debug_redacts_hook_env_values() {
        let settings = Settings {
            command: Some("make".to_owned()),
            release: HookSpec {
                command: Some("migrate".to_owned()),
                env: [("PASSWORD".to_owned(), "hunter2".to_owned())].into(),
            },
            ..Settings::default()
        };

        assert_eq!(
            format!("{settings:?}"),
            "Settings { command: Some(\"make\"), artifacts: [], reload: [], \
             release: HookSpec { command: Some(\"migrate\"), env: <1 vars> }, \
             after: HookSpec { command: None, env: <0 vars> } }"
        );
    }

    /// fails if a message carries a control character from a key the operator
    /// typed: serde names an unknown field in its message, and a TOML key can
    /// be any quoted string.
    #[test]
    fn a_parser_message_never_carries_a_control_character() {
        let app = fixtures::app_with_settings("web", "\"a\\u001bb\" = 1\n");

        let shown = from_app("web", &app).expect_err("refused").to_string();

        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.contains("a?b"), "{shown}");
    }
}
