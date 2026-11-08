//! The settings this dog deploys by: one type, read at two levels.
//!
//! `[deploy]` in `$SHEP_HOME/dogs.toml` is the value for every sheep this dog
//! serves. A sheep's own `[app.dogs.deploy]` table overrides any key of it
//! that is not dog-wide. `interval`, `retention`, `git_timeout`,
//! `build_timeout` and `passthrough` are dog-wide: one value per dog, refused
//! in a sheep's table rather than ignored there.
//!
//! ```toml
//! # dogs.toml. Every sheep shares a poll interval, a retention count and
//! # one branch.
//! [deploy]
//! interval = "10m"
//! retention = 3
//! branch = "main"
//! ```
//!
//! ```toml
//! # The sheep's own entry in shep.toml. It overrides `branch` and adds a
//! # build the other sheep do not have.
//! [app.dogs.deploy]
//! branch = "release"
//! command = "bun install && bun run build"
//! build_env = { VITE_SUPABASE_ANON_KEY = "{{secret:SUPABASE_ANON}}" }
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
//! Neither table lives in a release. A release's Flockfile is whatever the
//! tracked branch carried at its sha, so a setting read from it - the build
//! command above all - is chosen by whoever can land a commit. `build_env` and a hook's `env`
//! are marked `#[shep(secret)]`, so `shep lookout` shows `<set>` and never the
//! value, and a refusal to parse never quotes one either, see [`without_value`].
//!
//! `branch`, `watch` and `verify` used to live in `deploy.toml`. They are
//! filled into [`State`] by [`State::adopt`] and written back here by
//! [`change`], so the record holds state only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Map, Value};
use shep_client::dogs::DOG_WIDE_KEY;
use shep_client::shep_core::config::{AppConfig, DogTable};
use shep_client::shep_core::values::UpDuration;

use crate::build::{BuildSpec, contained_artifacts};
use crate::config::{
    DEFAULT_BUILD_TIMEOUT, DEFAULT_GIT_TIMEOUT, DEFAULT_INTERVAL, DEFAULT_RETENTION,
};
use crate::daemon::Daemon;
use crate::error::Error;
use crate::hook::{HookSpec, Hooks};
use crate::paths::{BuildKey, Tree};
use crate::shared::printable;
use crate::state::{State, Verify, Watch};

/// The name this dog's settings go by at both levels: `[deploy]` in
/// `dogs.toml`, and `deploy` under a sheep's `dogs`.
const TABLE: &str = "deploy";

/// The keys `#[shep(dog_wide)]` marks, read off the schema this dog publishes.
///
/// Read off the schema rather than written out again so the keys stripped from
/// the shared layer, the keys refused in a sheep's table, and the keys the
/// settings pane shows as dog-wide are one list and cannot drift.
///
/// Computed once: a poll tick reads every sheep's settings, and deriving the
/// schema each time would build the same document over and over.
fn dog_wide() -> &'static BTreeSet<String> {
    static KEYS: OnceLock<BTreeSet<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        shep_client::dogs::config_schema::<Settings>()
            .as_value()
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|properties| properties.iter())
            .filter(|(_, property)| property.get(DOG_WIDE_KEY) == Some(&Value::Bool(true)))
            .map(|(name, _)| name.clone())
            .collect()
    })
}

/// The dog's settings, as an operator types them at either level.
///
/// One type for both: `[deploy]` is the value for every sheep, and a sheep's
/// `[app.dogs.deploy]` table overrides any key of it that is not dog-wide.
/// Nothing merges the two by hand - [`DogTable::layered`](shep_client::shep_core::config::DogTable::layered)
/// does it once, per sheep.
///
/// This is also the type shep asks this binary about. [`crate::main`] hands it
/// to `shep_client::dogs::probe`, which answers `--schema` with the JSON Schema
/// `schemars` derives from it, and `shep lookout`'s settings pane draws a form
/// from that answer. Two consequences worth knowing before editing it:
///
/// - **The doc comment on each field below is what an operator reads** in that
///   pane, so they are written to be read there rather than here. The reasoning
///   about why a value is what it is stays on [`crate::config::DogConfig`],
///   which is what this crate's own code passes around.
/// - **The field names are the keys of both tables**, so renaming one is a
///   breaking change to hand-edited files whatever `deny_unknown_fields` is
///   doing.
///
/// The five dog-wide keys are read for one sheep as their defaults, never as
/// the value in force: `[deploy]`'s copy of them is taken out before a sheep's
/// table is layered over it, exactly as
/// [`DogRuntime::sheep_settings`](shep_client::dogs::DogRuntime::sheep_settings)
/// does.
/// The values in force are the dog's own, in [`crate::config::DogConfig`], and
/// they are validated once there rather than once per sheep.
///
/// Unknown keys are refused rather than ignored, as in [`BuildSpec`]: a typo in
/// `reload` would otherwise be a filter that is never applied, and one in
/// `command` a build that never runs, each with nothing saying so.
#[shep_client::dogs::dog_config]
#[derive(Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
// `rename` sets the root schema's title, which the settings pane heads its
// form with. `Settings` alone would name this crate's Rust type at an operator
// who is looking at a section of a TOML file.
#[schemars(rename = "deploy")]
// `description` overrides the doc comment above, which schemars would otherwise
// publish verbatim as the section's description: that text is addressed to
// whoever edits this file. The field docs need no such override - those were
// written for the pane in the first place.
#[schemars(
    description = "How the deploy dog polls, builds and verifies. Set a key here for every sheep \
                   this dog serves; a sheep's own [app.dogs.deploy] table overrides any key that \
                   is not dog-wide."
)]
pub struct Settings {
    /// How often to look for new commits, for example `"30s"`. A bare number
    /// is MILLISECONDS, so `30` is thirty milliseconds and not thirty seconds.
    /// At least one second. Dog-wide: a sheep cannot set it.
    #[shep(dog_wide)]
    pub(crate) interval: UpDuration,
    /// How many of the newest releases to keep per sheep. The live release and
    /// the one named in the sheep's `deploy.toml` are kept whatever their age,
    /// so a sheep can hold up to two more than this. At least two, since a
    /// rollback needs somewhere to go back to. Dog-wide: a sheep cannot set it.
    // The floor `DogConfig` refuses is stated to the pane as well so a section
    // can be rejected while it is being typed rather than at the next startup.
    #[shep(dog_wide)]
    #[schemars(range(min = 2))]
    pub(crate) retention: usize,
    /// How long any single git command may run before it is abandoned, for
    /// example `"5m"`. It bounds a remote that stops answering; a cold clone of
    /// a large repository legitimately runs minutes. At least one second.
    /// Dog-wide: a sheep cannot set it.
    #[shep(dog_wide)]
    pub(crate) git_timeout: UpDuration,
    /// How long a build, or a release hook, may run before it is abandoned, for
    /// example `"1h"`. It exists to turn a build that will never finish into an
    /// ordinary per-sheep failure, not to put a schedule on honest work, so it
    /// is set far longer than any build it is meant to bound. At least one
    /// second. Dog-wide: a sheep cannot set it.
    #[shep(dog_wide)]
    pub(crate) build_timeout: UpDuration,
    /// Environment variables to copy from this dog's own environment into a
    /// build, by name. A build otherwise starts from a cleared environment plus
    /// a small fixed set, so anything a build needs from the dog is named here
    /// and is visible in this file rather than inherited invisibly. Names,
    /// never values. Dog-wide: a sheep cannot set it.
    #[shep(dog_wide)]
    pub(crate) passthrough: Vec<String>,
    /// The branch polled for new commits, or a full ref beginning `refs/`.
    /// Nothing is deployed until some level sets one.
    pub branch: Option<String>,
    /// Whether the poll loop deploys new commits or only `shep-deploy deploy`
    /// does.
    pub watch: Watch,
    /// How a freshly deployed release is judged healthy.
    pub verify: Verify,
    /// The build command, run through `sh -c` inside the release. `None` is no
    /// build.
    pub command: Option<String>,
    /// Environment variables to set for the build, by name. They come after
    /// the fixed set and `passthrough`, so one of the same name wins. A value
    /// of `{{secret:KEY}}` is replaced with the secret `KEY` holds in the
    /// sheep's own environment, and a key with no secret refuses the deploy.
    #[shep(secret)]
    pub build_env: BTreeMap<String, String>,
    /// Paths, relative to the release, to copy back in after a successful
    /// build. See [`BuildSpec::artifacts`].
    #[serde(deserialize_with = "contained_artifacts")]
    pub artifacts: Vec<PathBuf>,
    /// Git pathspecs, relative to the repository root. A directory name matches
    /// its contents. Empty means every new sha deploys.
    pub reload: Vec<String>,
    /// Whether an unattended deploy may also skip a new commit whose build
    /// produced a `script` byte-identical to the one its last deploy landed.
    /// Off by default, and never for a deploy asked for by name. It fits a
    /// compiled app: an interpreted app's `script` misses the files it imports,
    /// and a Go build stamps the commit into the binary unless it passes
    /// `-buildvcs=false`.
    pub reload_script: bool,
    /// Runs after the build and before the swap.
    pub release: HookSpec,
    /// Runs once the new release is swapped in, verified and recorded.
    pub after: HookSpec,
}

/// `Debug` does not print `build_env`'s values (IR-41): after resolution they
/// are the secrets themselves. A hook's `env` is redacted by [`HookSpec`]'s own
/// `Debug`.
impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("interval", &self.interval)
            .field("retention", &self.retention)
            .field("git_timeout", &self.git_timeout)
            .field("build_timeout", &self.build_timeout)
            .field("passthrough", &self.passthrough)
            .field("branch", &self.branch)
            .field("watch", &self.watch)
            .field("verify", &self.verify)
            .field("command", &self.command)
            .field(
                "build_env",
                &format_args!("<{} vars>", self.build_env.len()),
            )
            .field("artifacts", &self.artifacts)
            .field("reload", &self.reload)
            .field("reload_script", &self.reload_script)
            .field("release", &self.release)
            .field("after", &self.after)
            .finish()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            retention: DEFAULT_RETENTION,
            git_timeout: DEFAULT_GIT_TIMEOUT,
            build_timeout: DEFAULT_BUILD_TIMEOUT,
            passthrough: Vec::new(),
            branch: None,
            watch: Watch::default(),
            verify: Verify::default(),
            command: None,
            build_env: BTreeMap::new(),
            artifacts: Vec::new(),
            reload: Vec::new(),
            reload_script: false,
            release: HookSpec::default(),
            after: HookSpec::default(),
        }
    }
}

impl Settings {
    /// The build these settings describe.
    pub(crate) fn build_spec(&self) -> BuildSpec {
        BuildSpec {
            command: self.command.clone(),
            env: self.build_env.clone(),
            artifacts: self.artifacts.clone(),
        }
    }

    /// The key of the release these settings build into, for an app built as
    /// `user` in a release linked out of `checkout`.
    ///
    /// The four things a release is keyed by, in one place, so the deploy that
    /// builds a release and the retention that reclaims it cannot disagree about
    /// which release a sheep is on.
    #[must_use]
    pub(crate) fn build_key(&self, user: Option<&str>, checkout: &Path) -> BuildKey {
        BuildKey::of(self.command.as_deref(), user, checkout, &self.build_env)
    }

    /// The hooks these settings describe.
    pub(crate) fn hooks(&self) -> Hooks {
        Hooks {
            release: self.release.clone(),
            after: self.after.clone(),
        }
    }
}

/// `section`, the dog's `[deploy]` table, as the layer every sheep's own is
/// put over: its dog-wide keys taken out, since those are the dog's own and a
/// sheep setting one is refused, not overridden.
pub(crate) fn shared(section: DogTable) -> DogTable {
    let wide = dog_wide();
    DogTable::from(
        section
            .into_map()
            .into_iter()
            .filter(|(key, _)| !wide.contains(key))
            .collect::<Map<String, Value>>(),
    )
}

/// `sheep`'s settings: `shared` under the `[app.dogs.deploy]` table the
/// shepherd holds for it, with every `{{secret:...}}` in that table resolved
/// in the sheep's own environment.
///
/// # Errors
/// Whatever [`Daemon::sheep_dog_table`] returns, and [`from_app`]'s.
pub(crate) async fn read<D: Daemon>(
    daemon: &D,
    sheep: &str,
    shared: &DogTable,
) -> Result<Settings, Error> {
    let own = daemon.sheep_dog_table(sheep, TABLE).await?;
    from_tables(sheep, shared, own.as_ref())
}

/// `sheep`'s whole stored config, and the settings layered onto it.
///
/// The config is the app a deploy builds as and verifies against: a release's
/// own files choose none of it. Its own table is read through
/// [`Daemon::sheep_dog_table`] rather than out of it, because that is the
/// request that resolves the table's secret references.
///
/// # Errors
/// As [`read`], plus whatever [`Daemon::sheep_config`] returns.
pub(crate) async fn held<D: Daemon>(
    daemon: &D,
    sheep: &str,
    shared: &DogTable,
) -> Result<(AppConfig, Settings), Error> {
    let app = daemon.sheep_config(sheep).await?;
    let own = daemon.sheep_dog_table(sheep, TABLE).await?;
    let settings = from_tables(sheep, shared, own.as_ref())?;
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
    shared: &DogTable,
) -> Result<(State, AppConfig, Settings), Error> {
    let mut state = State::read(&tree.state_file())?;
    let (app, settings) = held(daemon, tree.sheep(), shared).await?;
    state.adopt(tree.sheep(), &settings)?;
    Ok((state, app, settings))
}

/// `sheep`'s settings from the two tables, `shared` under `own`.
///
/// # Errors
/// [`Error::Config`] naming `sheep` and each key when `own` sets a dog-wide
/// key, and [`Error::Config`] naming `sheep` when the layered table does not
/// fit [`Settings`]: an unknown key, or a value of the wrong type.
pub(crate) fn from_tables(
    sheep: &str,
    shared: &DogTable,
    own: Option<&DogTable>,
) -> Result<Settings, Error> {
    if let Some(own) = own {
        let wide = dog_wide();
        let set: Vec<&str> = own
            .as_map()
            .keys()
            .filter(|key| wide.contains(*key))
            .map(String::as_str)
            .collect();
        if !set.is_empty() {
            return Err(Error::Config(format!(
                "{sheep}: `[app.dogs.deploy]` sets {}, which {} dog-wide. Set {} in `[deploy]` \
                 in dogs.toml, where every sheep this dog serves shares it",
                set.join(", "),
                if set.len() == 1 { "is" } else { "are" },
                if set.len() == 1 { "it" } else { "them" }
            )));
        }
    }
    let merged = match own {
        Some(own) => DogTable::layered(&[shared, own]),
        None => shared.clone(),
    };
    into_settings(sheep, merged)
}

/// `sheep`'s settings from its config's own table, with `shared` under it.
///
/// For a caller that already holds the sheep's config - the muster roll, and
/// the config a cutover is about to replace - and so need not ask the shepherd
/// for the table a second time.
///
/// What it reads is unresolved: a roll carries the table as the shepherd
/// stored it, `{{secret:...}}` references and all. So it is for a caller that
/// reports on the settings or builds from them, and never for one that runs a
/// hook. A hook's `env` is where an operator keeps a credential, and a
/// reference handed to `sh -c` is a wrong password on every deploy with nothing
/// pointing at the cause. [`read`] and [`held`] are the resolved reads.
///
/// # Errors
/// As [`from_tables`].
pub(crate) fn from_app(sheep: &str, shared: &DogTable, app: &AppConfig) -> Result<Settings, Error> {
    from_tables(sheep, shared, app.dogs.get(TABLE))
}

/// The layered table as [`Settings`].
///
/// # Errors
/// [`Error::Config`] naming `sheep` when the table does not fit [`Settings`].
fn into_settings(sheep: &str, layered: DogTable) -> Result<Settings, Error> {
    serde_json::from_value(Value::Object(layered.into_map())).map_err(|source| {
        Error::Config(format!(
            "{sheep}: `[app.dogs.deploy]` over `[deploy]` does not match the settings schema: {}",
            printable(without_value(&source.to_string()))
        ))
    })
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

/// Sets `changes` in `sheep`'s own `[app.dogs.deploy]` table and leaves every
/// other key of that table as the shepherd holds it.
///
/// What it reads back is the sheep's own table and never the layered one: a
/// value inherited from `[deploy]` written into a sheep's table is pinned there
/// for that sheep, and stops following the dog-wide value the moment the dog's
/// section changes.
///
/// The shepherd replaces a dog's table whole, so this reads the table, edits
/// it, and writes it back: an edit another client lands between the two is
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

/// `message` with the offending value cut out of a type or value error.
///
/// serde quotes what it could not use (`invalid type: string "hunter2", expected
/// a map`), and a hook's `env` is where the operator keeps secrets. An unknown
/// key is kept: naming it is the point of refusing it.
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
mod tests;
