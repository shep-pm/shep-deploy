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
//! branch = "main"
//! watch = "auto"
//! verify = "probed"
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
//! `branch`, `watch` and `verify` used to live in `deploy.toml`. They are
//! filled into [`State`] by [`State::adopt`] and written back here by
//! [`change`], so the record holds state only.
//!
//! No table at all is a sheep with no build, no hooks and no reload paths,
//! and one with no `branch`, which a deploy refuses until the operator names
//! one.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value};
use shep_client::shep_core::config::{AppConfig, DogTable};

use crate::build::{BuildSpec, contained_artifacts};
use crate::daemon::Daemon;
use crate::error::Error;
use crate::hook::{HookSpec, Hooks};
use crate::paths::Tree;
use crate::shared::printable;
use crate::state::{State, Verify, Watch};

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
    /// The branch polled for new commits, or a full ref beginning `refs/`.
    /// `None` is a sheep nobody has pointed at a branch yet.
    pub branch: Option<String>,
    /// Whether the poll loop deploys new commits or only `shep-deploy
    /// deploy` does.
    pub watch: Watch,
    /// How a freshly deployed release is judged healthy.
    pub verify: Verify,
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

/// `sheep`'s whole stored config, and the settings in its table.
///
/// The config is the app a deploy builds as and verifies against: a release's
/// own files choose none of it.
///
/// # Errors
/// As [`read`].
pub(crate) async fn held<D: Daemon>(
    daemon: &D,
    sheep: &str,
) -> Result<(AppConfig, Settings), Error> {
    let app = daemon.sheep_config(sheep).await?;
    let settings = from_app(sheep, &app)?;
    Ok((app, settings))
}

/// `tree`'s record with the settings it no longer holds adopted from the
/// shepherd, and the stored config and settings they came from.
///
/// # Errors
/// [`State::read`]'s, [`held`]'s and [`State::adopt`]'s.
pub(crate) async fn adopted<D: Daemon>(
    daemon: &D,
    tree: &Tree,
) -> Result<(State, AppConfig, Settings), Error> {
    let mut state = State::read(&tree.state_file())?;
    let (app, settings) = held(daemon, tree.sheep()).await?;
    state.adopt(tree.sheep(), &settings)?;
    Ok((state, app, settings))
}

/// One key of the table this dog writes, and its new value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// See [`Settings::branch`].
    Branch(String),
    /// See [`Settings::watch`].
    Watch(Watch),
    /// See [`Settings::verify`].
    Verify(Verify),
}

impl Change {
    fn entry(self) -> (&'static str, Value) {
        match self {
            Self::Branch(branch) => ("branch", Value::String(branch)),
            Self::Watch(watch) => ("watch", Value::String(watch.as_str().to_owned())),
            Self::Verify(verify) => ("verify", Value::String(verify.as_str().to_owned())),
        }
    }
}

/// Sets `changes` in `sheep`'s `[app.dogs.deploy]` table and leaves every
/// other key as the shepherd holds it.
///
/// The shepherd replaces a dog's table whole, so this reads the table, edits
/// it and writes it back: an edit another client lands between the two is
/// lost, the window shep's own settings pane has.
///
/// # Errors
/// As [`Daemon::sheep_config`] and [`Daemon::set_sheep_dog_settings`].
pub(crate) async fn change<D: Daemon>(
    daemon: &D,
    sheep: &str,
    changes: impl IntoIterator<Item = Change>,
) -> Result<(), Error> {
    let app = daemon.sheep_config(sheep).await?;
    let mut table: Map<String, Value> = app
        .dogs
        .get(TABLE)
        .map(|held| held.as_map().clone())
        .unwrap_or_default();
    for change in changes {
        let (key, value) = change.entry();
        table.insert(key.to_owned(), value);
    }
    daemon
        .set_sheep_dog_settings(sheep, TABLE, DogTable::from(table))
        .await
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
            "Settings { branch: None, watch: Auto, verify: Probed, \
             command: Some(\"make\"), artifacts: [], reload: [], \
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
