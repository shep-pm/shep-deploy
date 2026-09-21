//! The dog's own `[dog.<name>]` section: how often to poll, and how many
//! releases to keep.
//!
//! Deliberately the only two settings here, and deliberately nothing
//! per-target. Per-target state lives in the deploy tree's own
//! `deploy.toml` (see [`crate::state`]) because keying it to the dog's name
//! means renaming or re-adopting the dog destroys the record of every
//! deployment it manages, and those are unrelated things.
//!
//! Both values are refused rather than clamped when they name something
//! that cannot work. That is the same choice
//! [`crate::shared::shepignore_patterns`] makes about a glob and
//! [`crate::deploy`] makes about an ungated `probed` target: a setting that
//! silently does something other than what it says is a setting an operator
//! will believe in for as long as the disk lasts.

use std::time::Duration;

use serde::Deserialize;
use shep_client::shep_core::values::UpDuration;

use crate::daemon::{Daemon, adopted_name};
use crate::error::Error;

/// How often the poll loop looks for new commits, absent a config saying
/// otherwise.
///
/// Thirty seconds, settled in the design spec with its reasoning: adequate
/// for a single host, and it needs no inbound port, no HMAC verification
/// and no public exposure, which is the whole argument for polling over
/// webhooks at this scale.
const DEFAULT_INTERVAL: UpDuration = UpDuration::from_millis(30_000);

/// How many releases retention keeps, absent a config saying otherwise.
const DEFAULT_RETENTION: usize = 5;

/// How long a single git subprocess may run before it is abandoned, absent a
/// config saying otherwise.
///
/// Five minutes. The poll loop deploys targets one at a time on a
/// single-threaded runtime, so an unbounded git call does not fail a target,
/// it wedges every target and the smit refresh with it, silently. A bound
/// turns that into an ordinary per-target error the loop already knows how to
/// report and carry on from.
///
/// Five rather than something tighter because a cold clone of a large
/// repository legitimately runs minutes, and failing honest work is worse
/// than a late failure on a remote that was never going to answer.
const DEFAULT_GIT_TIMEOUT: UpDuration = UpDuration::from_millis(300_000);

/// How long a build may run before it is abandoned.
///
/// An hour, which is far longer than any build this is meant to bound and is
/// deliberate. The purpose is to turn a build that will NEVER finish into an
/// ordinary per-target failure, not to put a schedule on honest work: a cold
/// Rust build of a large workspace legitimately runs tens of minutes, and
/// failing one of those would be a worse bug than the one this fixes.
///
/// Without it a build that hangs stops the whole dog, not just its own target.
/// `crate::poll::tick` deploys targets one at a time, so a single hung build
/// holds the loop forever: no other target deploys, no smit refreshes, and
/// nothing is logged, because nothing has failed. That is exactly the shape
/// `git_timeout` was added for, one subprocess over.
const DEFAULT_BUILD_TIMEOUT: UpDuration = UpDuration::from_millis(3_600_000);

/// The fewest releases a target can keep and still be able to roll back.
///
/// Two: the one that is live, and the one before it. Retention keeps the
/// newest N, so N of one prunes the rollback target.
const MINIMUM_RETENTION: usize = 2;

/// The shortest interval, git timeout or build timeout accepted.
///
/// One second, and the number is less the point than the unit. `UpDuration`
/// reads a bare number as MILLISECONDS, so `interval = "30"`, the obvious
/// hand-edit for thirty seconds, is thirty milliseconds: a dog fetching
/// thirty times a second, which is the exact harm the zero refusal below
/// names, one keystroke away. Anything under a second is that mistake and
/// not a setting anybody meant.
const MINIMUM_DURATION: Duration = Duration::from_secs(1);

/// The `[dog.<name>]` section, parsed.
///
/// `Debug` is derived: a poll interval and a count are not secrets, and
/// unlike the raw section text this type never holds the whole table. The
/// raw text is a different matter and `crate::daemon::named` already
/// refuses to print it, because a `[dog.<name>]` section routinely carries
/// webhook credentials for other dogs.
///
/// Not `Copy`: `passthrough` is a `Vec`. Cloning a config once per tick is
/// not a cost worth designing around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogConfig {
    /// How long the poll loop sleeps between ticks.
    pub interval: Duration,
    /// How many of the newest releases per target retention keeps.
    ///
    /// The release `current` names and the one `deploy.toml` names are
    /// spared whatever their age, so a target holds `retention` directories
    /// in the ordinary case and up to two more after a deploy that died
    /// between its swap and its record write. See `crate::retention` for
    /// why those two are named rather than counted.
    pub retention: usize,
    /// How long any single git subprocess may run before it is abandoned.
    pub git_timeout: Duration,
    /// How long a build may run before it is abandoned.
    pub build_timeout: Duration,
    /// Environment variables copied from this process into a build, by name.
    ///
    /// A build otherwise starts from a cleared environment plus a small fixed
    /// set (see `crate::build::BASE_ENV`), so anything a build needs from the
    /// dog's own environment is opted into here by name and is visible in
    /// `dogs.toml` rather than inherited invisibly.
    pub passthrough: Vec<String>,
}

/// The `[deploy]` section as an operator types it, kept separate from
/// [`DogConfig`] so the validated type cannot be constructed without going
/// through [`DogConfig::parse`].
///
/// Also the type shep asks this binary about. `crate::main` hands it to
/// `shep_client::dogs::probe`, which answers `--schema` with the JSON
/// Schema `schemars` derives from it, and `shep lookout`'s settings pane
/// draws a form from that answer. Two consequences worth knowing before
/// editing this type:
///
/// - **The doc comment on each field below is what an operator reads** in
///   that pane, so they are written to be read there rather than here.
///   The reasoning about why a value is what it is stays on [`DogConfig`],
///   which is the type this crate's own code passes around.
/// - **The field names are the section's keys**, so renaming one is a
///   breaking change to a hand-edited file whatever `deny_unknown_fields`
///   is doing.
///
/// No field carries `#[shep(secret)]`, and none should: an interval, a
/// count and a list of variable NAMES are not credentials. `passthrough`
/// is the one worth pausing on, and it names variables rather than
/// carrying their values - the values stay in the dog's environment and
/// never enter this file. The derive is here all the same, because a
/// config type with nothing to mark still needs the impl for shep to have
/// a schema to ask for at all. It is also why `Debug` is derived rather
/// than hand-written to redact something (IR-41): there is nothing here to
/// redact.
#[derive(Debug, Deserialize, schemars::JsonSchema, shep_client::dogs::DogConfig)]
#[serde(default, deny_unknown_fields)]
// `rename` sets the root schema's title, which the settings pane heads its
// form with. `Section` alone would name this crate's Rust type at an
// operator who is looking at a section of a TOML file.
#[schemars(rename = "deploy")]
// `description` overrides the doc comment above, which schemars would
// otherwise publish verbatim as the section's description: that text is
// addressed to whoever edits this file, and it would arrive in the settings
// pane as several paragraphs about Rust types. The field docs need no such
// override - those were written for the pane in the first place.
#[schemars(
    description = "How the deploy dog polls, how long it lets git and a build run, and how many \
                   releases it keeps. Every target this dog manages shares these; what to deploy \
                   and where lives in each target's own deploy.toml."
)]
pub struct Section {
    /// How often to look for new commits, for example `"30s"`. A bare
    /// number is MILLISECONDS, so `30` is thirty milliseconds and not
    /// thirty seconds. At least one second.
    interval: UpDuration,
    /// How many of the newest releases to keep per target. The live release
    /// and the one named in the target's `deploy.toml` are kept whatever
    /// their age, so a target can hold up to two more than this. At least
    /// two, since a rollback needs somewhere to go back to.
    // The floor `DogConfig::parse` refuses below, stated to the pane as
    // well so a section can be rejected while it is being typed rather
    // than at the next startup.
    #[schemars(range(min = 2))]
    retention: usize,
    /// How long any single git command may run before it is abandoned, for
    /// example `"5m"`. It bounds a remote that stops answering; a cold
    /// clone of a large repository legitimately runs minutes. At least one
    /// second.
    git_timeout: UpDuration,
    /// How long a build may run before it is abandoned, for example
    /// `"1h"`. It exists to turn a build that will never finish into an
    /// ordinary per-target failure, not to put a schedule on honest work,
    /// so it is set far longer than any build it is meant to bound. At
    /// least one second.
    build_timeout: UpDuration,
    /// Environment variables to copy from this dog's own environment into
    /// a build, by name. A build otherwise starts from a cleared
    /// environment plus a small fixed set, so anything a build needs from
    /// the dog is named here and is visible in this file rather than
    /// inherited invisibly. Names, never values.
    passthrough: Vec<String>,
}

impl Default for Section {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            retention: DEFAULT_RETENTION,
            git_timeout: DEFAULT_GIT_TIMEOUT,
            build_timeout: DEFAULT_BUILD_TIMEOUT,
            passthrough: Vec::new(),
        }
    }
}

impl DogConfig {
    /// Parses a `[dog.<name>]` section's body.
    ///
    /// An empty string is the ordinary case, not an edge one:
    /// [`Daemon::dog_config`] answers an absent section that way, and a dog
    /// adopted without configuration is the common shape.
    ///
    /// # Errors
    /// [`Error::Config`] if the text is not valid TOML, carries a key this
    /// dog does not know, gives a value of the wrong type, asks for a
    /// retention below two, asks for an `interval`, `git_timeout` or
    /// `build_timeout` under a second, or names a `passthrough` entry that
    /// is empty, repeated, or not an environment variable name.
    ///
    /// Every case except the first names the offending key, because the
    /// section is one an operator edited by hand and a complaint they cannot
    /// locate is nearly as bad as none. A plain syntax error is the
    /// exception and names a line and column instead, because at that point
    /// the parser has no key to name.
    pub fn parse(toml: &str) -> Result<Self, Error> {
        let raw: Section = toml::from_str(toml)
            .map_err(|source| Error::Config(format!("[dog.<name>]: {source}")))?;

        if raw.retention < MINIMUM_RETENTION {
            return Err(Error::Config(format!(
                "retention = {} keeps too few releases to roll back: the release a failed \
                 deploy returns to is the second newest, so anything below {MINIMUM_RETENTION} \
                 prunes the only thing there is to roll back to",
                raw.retention
            )));
        }

        let interval = at_least_a_second(
            "interval",
            raw.interval,
            "would fetch continuously rather than on a schedule, which reads as a hung dog and \
             hammers the remote it is watching",
        )?;
        let git_timeout = at_least_a_second(
            "git_timeout",
            raw.git_timeout,
            "would abandon every fetch the moment it started; the point of the bound is to \
             turn a hung remote into an ordinary per-target failure, not to disable git",
        )?;
        let build_timeout = at_least_a_second(
            "build_timeout",
            raw.build_timeout,
            "would abandon every build the moment it started; the point of the bound is to \
             turn a build that never finishes into an ordinary per-target failure, not to \
             disable building",
        )?;

        let mut seen = std::collections::BTreeSet::new();
        for name in &raw.passthrough {
            let why = if name.is_empty() {
                Some("is empty")
            } else if name.contains(['=', '\0']) {
                Some("is not an environment variable name: one cannot contain `=` or NUL")
            } else if !seen.insert(name.as_str()) {
                Some("is listed twice")
            } else {
                None
            };
            if let Some(why) = why {
                return Err(Error::Config(format!(
                    "passthrough entry {name:?} {why}; each entry names one variable to copy \
                     from the dog's environment into a build"
                )));
            }
        }

        Ok(Self {
            interval,
            retention: raw.retention,
            git_timeout,
            build_timeout,
            passthrough: raw.passthrough,
        })
    }
}

/// `value` as a [`Duration`], refused by `key` if it is under
/// [`MINIMUM_DURATION`].
///
/// The refusal spells the value out in milliseconds and says what a bare
/// number means, because that is the mistake it exists to catch: `"30"` is
/// thirty milliseconds, and the operator meant `"30s"`.
///
/// # Errors
/// [`Error::Config`] naming `key`, the value as read, and `harm`.
fn at_least_a_second(key: &str, value: UpDuration, harm: &str) -> Result<Duration, Error> {
    let duration = value.as_duration();
    if duration < MINIMUM_DURATION {
        return Err(Error::Config(format!(
            "{key} = {}ms is under a second, which {harm}. A bare number is read as \
             milliseconds: write `{key} = \"30s\"` for thirty seconds",
            duration.as_millis()
        )));
    }
    Ok(duration)
}

/// This dog's own section, read from the shepherd.
///
/// A dog that cannot work out the name it was adopted under gets the
/// documented defaults rather than an error, which is
/// [`adopted_name`]'s own contract: not knowing the name and being adopted
/// under a name with no section are the same position from here.
///
/// The one-shot verbs use this. The poll loop uses [`Reader`], which is
/// this plus the name it resolved, so that it can ask again every tick
/// without listing the flock again every tick.
///
/// # Errors
/// Whatever [`Daemon::dog_config`] returns, plus [`Error::Config`] from
/// [`DogConfig::parse`].
pub async fn read<D: Daemon>(daemon: &D) -> Result<DogConfig, Error> {
    match adopted_name(daemon).await {
        Some(name) => section_of(daemon, &name).await,
        None => DogConfig::parse(""),
    }
}

/// The section registered under `name`, parsed.
///
/// # Errors
/// As [`read`].
async fn section_of<D: Daemon>(daemon: &D, name: &str) -> Result<DogConfig, Error> {
    let section = daemon.dog_config(name).await?;
    DogConfig::parse(&section)
}

/// This dog's own section, and the means to ask for it again.
///
/// The poll loop refreshes this at the top of every tick, so an `interval`
/// or a `retention` an operator changes is picked up by the next tick,
/// without a restart. By the next tick rather than within one interval:
/// the refresh comes before the tick and the sleep comes after it, so an
/// edit written just as a deploy starts waits for that deploy to finish
/// and then for the sleep the OLD interval asked for.
/// `shep lookout` writes the section and
/// then says the dog has been told; before this type existed that sentence
/// was true about the shepherd and false about the dog, and nothing said
/// so.
///
/// # Why the name is resolved once and the section is not
///
/// [`adopted_name`] costs a whole flock listing, and the answer cannot
/// change under a running process: shep spawns an adopted dog itself and
/// names it then, so a rename is a respawn. The section is the part that
/// an operator edits while the dog runs, so it is the part worth asking
/// for again.
///
/// A dog that had no name to resolve stays on the documented defaults and
/// asks for nothing at all. That is the same dog [`read`] describes, and a
/// process nothing adopted has no section to be told about.
///
/// `Debug` is derived rather than redacted (IR-41), because neither field
/// is a secret: the name is what `shep dogs` already prints, and
/// [`DogConfig`]'s own doc says why an interval and a count are safe. The
/// raw section text is the part that would need redacting and it never
/// reaches this type, which is the same reason `DogConfig` gives.
#[derive(Debug)]
pub struct Reader {
    /// The name shep adopted this dog under, or `None` when nothing did.
    name: Option<String>,
    /// The newest section that parsed.
    current: DogConfig,
}

impl Reader {
    /// Resolves this dog's name and reads its section, once, as it starts.
    ///
    /// # Errors
    /// As [`read`]. A dog does not start on a section it cannot read; see
    /// [`Self::refresh`] for why that is not what a later read does.
    pub async fn open<D: Daemon>(daemon: &D) -> Result<Self, Error> {
        let name = adopted_name(daemon).await;
        let current = match &name {
            Some(name) => section_of(daemon, name).await?,
            None => DogConfig::parse("")?,
        };
        Ok(Self { name, current })
    }

    /// The newest section that parsed.
    pub const fn current(&self) -> &DogConfig {
        &self.current
    }

    /// Reads the section again, answering with the complaint when it could
    /// not be read or would not parse.
    ///
    /// # Why this keeps the last section rather than ending the dog
    ///
    /// [`Self::open`] propagates and the dog exits, because a dog running
    /// on defaults it was not asked for looks exactly like one honouring
    /// the config and there is nothing else for it to run on. Neither half
    /// holds here. The section that parsed a tick ago is something else to
    /// run on, and it is one the operator asked for rather than a default.
    /// And a tick of THIS dog can be a whole deploy - a fetch, a build and
    /// a reload of a live app - so ending one over a half-typed edit in a
    /// file somebody is still in the middle of costs more than it does in a
    /// dog that rotates logs.
    ///
    /// What the two answers have in common is the part that matters: the
    /// dog never quietly runs on something nobody asked for. The caller
    /// says so every time, through the same mute every other repeated
    /// complaint goes through, so a section left broken is said once and
    /// then hourly rather than once and then never.
    ///
    /// # Errors
    /// Answers `Some` with whatever [`Daemon::dog_config`] returns when the
    /// shepherd cannot be reached or refuses, and with [`Error::Config`]
    /// from [`DogConfig::parse`] when the section it answers with will not
    /// parse. `None` is both answers that are not a complaint: a section
    /// that parsed, and a dog with no name to ask about.
    pub async fn refresh<D: Daemon>(&mut self, daemon: &D) -> Option<Error> {
        // No name is no complaint, which is what the `?` answers here: a
        // process no shepherd adopted has no section to be told about, and
        // asking again every tick would list the whole flock forever to be
        // told so. Cloned because `self` is borrowed mutably for the write
        // below, across the await.
        let name = self.name.clone()?;
        match section_of(daemon, &name).await {
            Ok(fresh) => {
                self.current = fresh;
                None
            }
            Err(err) => Some(err),
        }
    }

    /// A reader that starts from `current`, for tests whose subject is
    /// something other than the reading.
    ///
    /// A `None` name is a reader [`Self::refresh`] asks nothing for, which
    /// is what the loop's older tests want: they pin behaviour against a
    /// config they set by hand, and a double that had to serve a section
    /// too would put the thing under test behind a fixture.
    #[cfg(test)]
    pub const fn primed(name: Option<String>, current: DogConfig) -> Self {
        Self { name, current }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if an empty or absent section stops meaning "the documented
    /// defaults". A dog is adopted with no config at all in the ordinary
    /// case, and `Daemon::dog_config` answers an absent section with an
    /// empty string rather than an error, so this is the path almost every
    /// real dog takes.
    #[test]
    fn an_empty_section_is_the_documented_defaults() {
        let config = DogConfig::parse("").expect("an empty section parses");
        assert_eq!(config.interval, Duration::from_secs(30));
        assert_eq!(config.retention, 5);
    }

    /// fails if either key stops being read. Both are the only reason this
    /// module exists, and a config silently running on defaults looks
    /// exactly like a config being honoured.
    #[test]
    fn both_keys_are_read() {
        let config = DogConfig::parse("interval = \"5m\"\nretention = 12").expect("parses");
        assert_eq!(config.interval, Duration::from_secs(300));
        assert_eq!(config.retention, 12);
    }

    /// fails if a retention count below two is accepted. It would silently
    /// disable rollback: retention keeps the newest N releases, and the
    /// rollback target IS the second newest, so `retention = 1` prunes the
    /// only thing a failed deploy can return to. Refused loudly at parse
    /// time rather than clamped, matching how `.shepignore` refuses a glob:
    /// an operator who asked for something that cannot work should be told,
    /// not quietly given something else.
    #[test]
    fn a_retention_below_two_is_refused_by_name() {
        for count in ["0", "1"] {
            let err = DogConfig::parse(&format!("retention = {count}")).expect_err("refuses");
            let shown = err.to_string();
            assert!(shown.contains("retention"), "{shown}");
            assert!(shown.contains("roll back"), "{shown}");
        }
    }

    /// fails if a zero interval is accepted. A poll loop sleeping zero
    /// between ticks fetches continuously, which is a denial of service
    /// against the operator's own git remote and reads as a hung dog.
    #[test]
    fn a_zero_interval_is_refused() {
        let err = DogConfig::parse("interval = \"0s\"").expect_err("refuses");
        assert!(err.to_string().contains("interval"), "{err}");
    }

    /// fails if a bare number is accepted as seconds. `UpDuration` reads
    /// `"30"` as thirty MILLISECONDS, so the obvious hand-edit for the
    /// documented default fetched thirty times a second and was accepted
    /// because it was not zero. The refusal has to name the key, say what
    /// the number was read as, and show the spelling that was meant.
    #[test]
    fn a_duration_under_a_second_is_refused_and_the_unit_is_named() {
        for key in ["interval", "git_timeout", "build_timeout"] {
            let err = DogConfig::parse(&format!("{key} = \"30\"")).expect_err(key);
            let shown = err.to_string();
            assert!(shown.contains(key), "{shown}");
            assert!(
                shown.contains("30ms"),
                "must say what it was read as: {shown}"
            );
            assert!(
                shown.contains("\"30s\""),
                "must show the spelling meant: {shown}"
            );
        }
        // And a second exactly is the floor, not past it.
        DogConfig::parse("interval = \"1s\"\ngit_timeout = \"1s\"\nbuild_timeout = \"1s\"")
            .expect("one second is allowed");
    }

    /// fails if the two timeouts and the passthrough list stop being read.
    /// `both_keys_are_read` predates all three, and a config silently
    /// running on defaults looks exactly like one being honoured.
    #[test]
    fn the_timeouts_and_passthrough_are_read() {
        let config = DogConfig::parse(
            "git_timeout = \"2m\"\nbuild_timeout = \"3h\"\npassthrough = [\"CARGO_HOME\", \"NPM_TOKEN\"]",
        )
        .expect("parses");
        assert_eq!(config.git_timeout, Duration::from_secs(120));
        assert_eq!(config.build_timeout, Duration::from_secs(3 * 3600));
        assert_eq!(config.passthrough, vec!["CARGO_HOME", "NPM_TOKEN"]);
    }

    /// fails if a passthrough entry that cannot name a variable is accepted.
    /// `passthrough` is the only door for a registry token, and an entry
    /// that can never match anything is dropped in silence at build time,
    /// which this module's own doc says is the wrong shape for a setting.
    #[test]
    fn a_passthrough_entry_that_is_not_a_variable_name_is_refused() {
        for entry in ["\"\"", "\"A=B\"", "\"X\", \"X\""] {
            let err = DogConfig::parse(&format!("passthrough = [{entry}]")).expect_err(entry);
            let shown = err.to_string();
            assert!(shown.contains("passthrough"), "{shown}");
        }
    }

    /// fails if a typo is ignored instead of refused. `retenton = 2` that
    /// parses to the default of 5 is a config an operator will believe is
    /// in force for as long as the disk lasts. Same reasoning as
    /// `BuildSpec`'s own `deny_unknown_fields`.
    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let err = DogConfig::parse("retenton = 2").expect_err("refuses");
        assert!(err.to_string().contains("retenton"), "{err}");
    }

    /// fails if a wrong-typed value produces a message that does not name
    /// the key. This section is hand-edited in `dogs.toml`, and
    /// `retention = "five"` is a plausible thing to write; a bare "invalid
    /// type: string" with no key names nothing an operator can go and fix.
    #[test]
    fn a_value_of_the_wrong_type_names_the_key() {
        let err = DogConfig::parse("retention = \"five\"").expect_err("refuses");
        assert!(err.to_string().contains("retention"), "{err}");
    }

    /// fails if a `#[shep(secret)]` mark ever names a field the schema does
    /// not have, which `#[serde(rename)]` on a marked field is what
    /// produces. That combination is not a compile error and not a bad
    /// schema: `probe` prints the complaint to stderr and exits 1, so shep
    /// reads it as a dog whose schema is unreadable and adopts it anyway,
    /// with the credential's field left unmarked. Nothing on the happy path
    /// says so, which is why it is asserted here rather than left to the
    /// first operator who opens the pane.
    #[test]
    fn the_section_renders_a_schema_with_every_mark_landing() {
        shep_client::dogs::config_schema::<Section>()
            .expect("every `#[shep(secret)]` field names a property of this type");
    }

    /// fails if the schema stops describing the keys `parse` accepts.
    ///
    /// The two come from one struct and cannot drift by accident, but they
    /// can by edit: a `#[serde(rename)]` moves both together and a
    /// `#[schemars(rename)]` moves only one. The consequence is specific
    /// and quiet. `shep lookout`'s settings pane writes the section from
    /// the schema's property names, and the section is
    /// `deny_unknown_fields`, so a name only the schema knows produces a
    /// `dogs.toml` this dog refuses at its next startup - written by shep's
    /// own form, on a key the operator never typed.
    #[test]
    fn every_property_the_schema_publishes_is_a_key_the_parser_takes() {
        let schema = shep_client::dogs::config_schema::<Section>().expect("renders");
        let schema = schema.as_value();
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("a derived struct schema has properties");

        assert_eq!(
            properties.len(),
            5,
            "a property was added or removed without this test being read"
        );
        for name in properties.keys() {
            // The value is deliberately one no field is required to accept:
            // an unknown key is refused before any value is looked at, so
            // whether this one type-checks is beside the point and only the
            // `unknown field` half is asserted. `passthrough = []` really is
            // valid, and a key that parses is a key the parser knows.
            if let Err(err) = DogConfig::parse(&format!("{name} = []")) {
                assert!(
                    !err.to_string().contains("unknown field"),
                    "the schema publishes `{name}`, which the parser does not know: {err}"
                );
            }
        }
    }

    /// fails if the floor the schema advertises stops being the floor the
    /// parser enforces. The pane refuses below `minimum` while the operator
    /// is typing; `parse` refuses below `MINIMUM_RETENTION` at startup. Two
    /// numbers, one rule, and the failure of the pair is a form that
    /// cheerfully writes a section the dog will not boot on.
    #[test]
    fn the_schemas_retention_floor_is_the_one_the_parser_enforces() {
        let schema = shep_client::dogs::config_schema::<Section>().expect("renders");
        let minimum = schema
            .as_value()
            .pointer("/properties/retention/minimum")
            .and_then(serde_json::Value::as_u64)
            .expect("retention states a minimum");

        assert_eq!(minimum, MINIMUM_RETENTION as u64);
        DogConfig::parse(&format!("retention = {minimum}")).expect("the floor itself is accepted");
        DogConfig::parse(&format!("retention = {}", minimum - 1)).expect_err("below it is not");
    }

    /// The name a test's shepherd has adopted this dog under.
    const ADOPTED: &str = "deploy";

    /// A reader that will really ask for its section, starting from the
    /// documented defaults.
    fn reading() -> Reader {
        Reader::primed(
            Some(ADOPTED.to_owned()),
            DogConfig::parse("").expect("the defaults"),
        )
    }

    /// fails if a section read again stops replacing the one before it, or
    /// replaces only part of it. This is the whole of what makes an edit
    /// reach a running dog: `crate::poll` passes `current` to every tick
    /// and sleeps on its `interval`, so a value that does not land here
    /// lands nowhere.
    #[tokio::test]
    async fn a_refresh_that_parses_replaces_the_whole_section() {
        let daemon = crate::fixtures::Sections::of(&["interval = \"5m\"\nretention = 9"]);
        let mut reader = reading();

        assert!(reader.refresh(&daemon).await.is_none(), "it parsed");

        assert_eq!(reader.current().interval, Duration::from_secs(300));
        assert_eq!(reader.current().retention, 9);
    }

    /// fails if a section that stops parsing takes the one the dog was
    /// working on with it.
    ///
    /// The startup read refuses to start on a section it cannot parse, and
    /// this deliberately answers the same mistake differently: there is a
    /// section that parsed to fall back on by now, it is one the operator
    /// asked for rather than a default, and a tick of this dog can be a
    /// whole deploy. The complaint is how it stays honest about that, so
    /// the error comes back rather than being swallowed.
    #[tokio::test]
    async fn a_refresh_that_fails_keeps_the_last_section_that_parsed() {
        let daemon = crate::fixtures::Sections::of(&["retention = 9", "retention = 1"]);
        let mut reader = reading();

        assert!(reader.refresh(&daemon).await.is_none(), "the first parsed");
        assert_eq!(reader.current().retention, 9);

        let complaint = reader
            .refresh(&daemon)
            .await
            .expect("the second is refused");
        assert!(
            complaint.to_string().contains("keeps too few releases"),
            "{complaint}"
        );
        assert_eq!(reader.current().retention, 9, "still the one that parsed");
    }

    /// fails if `Reader`'s `Debug` starts carrying something an operator
    /// would not want in a log. The derive is safe only because the raw
    /// section text never reaches this type, so this pins the shape: a
    /// name, and a `DogConfig` of parsed values.
    #[test]
    fn debug_shows_the_name_and_the_parsed_values_only() {
        let reader = Reader::primed(
            Some(ADOPTED.to_owned()),
            DogConfig::parse("retention = 9").expect("a section"),
        );

        let shown = format!("{reader:?}");

        assert!(shown.contains("name: Some(\"deploy\")"), "{shown}");
        assert!(shown.contains("retention: 9"), "{shown}");
        assert!(!shown.contains("Section"), "no raw section text: {shown}");
    }

    /// fails if a dog nothing adopted starts asking the shepherd for a
    /// section every tick. There is no section to ask for - that is what
    /// having no name means here - and the ask is a whole flock listing,
    /// forever, to be told so.
    #[tokio::test]
    async fn an_unnamed_dog_asks_for_no_section_at_all() {
        let daemon = crate::fixtures::Sections::of(&["retention = 9"]);
        let mut reader = Reader::primed(None, crate::fixtures::dog_config());

        assert!(reader.refresh(&daemon).await.is_none());

        assert_eq!(daemon.asked(), 0, "nothing was asked for");
        assert_eq!(reader.current(), &crate::fixtures::dog_config());
    }
}
