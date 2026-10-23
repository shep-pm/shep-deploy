//! `deploy.toml`: the only record of how a sheep ran before this dog took it
//! over.
//!
//! It lives in the deploy tree (see [`crate::paths::Tree::state_file`]),
//! keyed to the sheep, not to the dog's own `[dog.<name>]` section in
//! `shep.toml`. Keying it to the dog's name would mean renaming or
//! re-adopting the dog destroys the record of every deployment it manages -
//! unrelated things. Kept here, it survives rehoming and makes the tree
//! self-describing on its own.
//!
//! The file holds state only. What an operator chooses, the branch followed,
//! `watch` and `verify`, lives in the sheep's `[app.dogs.deploy]` table (see
//! [`crate::settings`]) and is filled into the matching fields of [`State`]
//! by [`State::adopt`]; those fields are never written here.
//!
//! [`State::origin_cwd`] and [`State::origin_script`] are why removing the
//! dog can put a sheep back where its operator will look for it: they are
//! the `cwd` and script the sheep ran with *before* adoption, captured once
//! at opt-in and never touched again.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shep_client::shep_core::atomic_file::{create_staging_file, sync_dir};
use shep_client::shep_core::config::AppConfig;

use crate::error::Error;
use crate::settings::Settings;

/// The keys `deploy.toml` used to carry and the sheep's table holds now.
const MOVED: [&str; 3] = ["branch", "watch", "verify"];

/// How a freshly deployed release is judged healthy before traffic (or the
/// old release) is torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verify {
    /// Wait for the app's configured readiness probe to answer before
    /// calling the release good. The default: an absent `verify` key must
    /// never silently mean "deploy without checking".
    #[default]
    Probed,
    /// Skip the probe; a release that stays running for the grace period is
    /// good enough.
    Alive,
}

impl Verify {
    /// The word `[app.dogs.deploy]` spells this mode.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Probed => "probed",
            Self::Alive => "alive",
        }
    }
}

/// Whether this sheep is polled for new commits automatically, or only
/// deployed on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Watch {
    /// The poll loop deploys new commits on the tracked branch as they
    /// appear. The default: a target written before this field existed, or
    /// by hand without it, keeps being watched rather than silently
    /// stopping.
    #[default]
    Auto,
    /// The poll loop skips this target entirely. It deploys only when asked
    /// (`shep deploy <sheep>`). Everything else - build, atomic swap, probe
    /// verification, auto-rollback - still applies; only the trigger
    /// changes.
    Manual,
}

impl Watch {
    /// The word `[app.dogs.deploy]` spells this mode.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// `deploy.toml`'s full contents, plus the three settings a deploy holds in
/// memory beside it.
///
/// [`Self::branch`], [`Self::watch`] and [`Self::verify`] are the sheep's
/// `[app.dogs.deploy]` table as of the last [`Self::adopt`]. They are skipped
/// by serde, so they are neither written nor accepted: a record carrying one
/// is refused by [`Self::read`], and `deny_unknown_fields` is what keeps an
/// older reader of this file from silently dropping a key a newer one wrote.
/// A `State` fresh from [`Self::read`] holds the defaults until adopted.
///
/// Every other field here is load-bearing for restoring the sheep if the dog is
/// ever removed: losing `origin_cwd` or `origin_script` means removal
/// cannot put the app back where its operator will look for it, and it is
/// left running from a path under `$SHEP_HOME` they have no reason to know
/// about.
///
/// `Debug` is derived, and since 2026-09-04 that rests on one thing:
/// [`Self::origin`] carries the app's `env` verbatim, and `AppConfig`'s own
/// hand-written `Debug` prints that as `<N vars>` (shep-core, IR-41). Every
/// other field is a git URL an operator already has in their checkout, a
/// path, a sha, a shell command line their own `shep.toml` already held in
/// plaintext, or one of the two small enums above. A field added here that
/// could carry a secret needs its own redaction; the derive is not a
/// blanket exemption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The git remote this sheep is deployed from.
    pub remote: String,
    /// The branch polled for new commits, or a full ref beginning `refs/`,
    /// such as `refs/pull/1/head`, to follow that ref instead.
    ///
    /// A ref outside `refs/heads/` is fetched by name alongside the branch
    /// mirror, so only that one ref is. From the sheep's table, empty until
    /// [`Self::adopt`]; never serialised.
    #[serde(skip)]
    pub branch: String,
    /// The sha currently live under `current`, or `None` before the first
    /// successful deploy.
    pub deployed: Option<String>,
    /// The sha an attempt last failed on, or `None` when the last attempt
    /// landed.
    ///
    /// What stops the poll loop rebuilding one bad commit forever.
    /// `deployed` advances only after a verify, so without this the loop
    /// finds the branch head different from what is serving on every
    /// single tick and runs the whole sequence again: fetch, full rebuild,
    /// swap, reload, wait out the verification budget, roll back, reload
    /// again. Two reloads of a live app and a build every thirty seconds,
    /// indefinitely, from one bad commit.
    ///
    /// The branch moving is what clears it, which is what CI does with a
    /// red commit. That is the wrong answer for a deploy that failed on a
    /// network blip rather than on the commit, and it was chosen with that
    /// in mind: it costs a push, or one `shep deploy <sheep>`, which
    /// retries a held sha deliberately.
    #[serde(default)]
    pub failed: Option<String>,
    /// How a freshly deployed release is judged healthy. From the sheep's
    /// table; never serialised.
    #[serde(skip)]
    pub verify: Verify,
    /// Whether this sheep is polled automatically or only deploys on
    /// request. From the sheep's table; never serialised.
    #[serde(skip)]
    pub watch: Watch,
    /// The working directory the sheep ran from before adoption, for
    /// restore on removal. `None` when the dog itself bootstrapped the
    /// sheep, so there is nothing to restore it to.
    pub origin_cwd: Option<PathBuf>,
    /// The script/command the sheep ran before adoption, for restore on
    /// removal. `None` alongside `origin_cwd` for a dog-bootstrapped sheep.
    pub origin_script: Option<String>,
    /// The user's own git checkout this deploy was set up from. Read-only:
    /// the dog clones from it once at opt-in and never restructures, moves,
    /// or writes to it again.
    pub checkout: PathBuf,
    /// The whole app definition as the shepherd had it BEFORE adoption, so
    /// removal can put back everything and not only `cwd` and `script`.
    ///
    /// `origin_cwd` and `origin_script` predate this field and are kept for
    /// the records that carry only them; a record with both is restored
    /// from this one. It is what makes `deploy.toml` hold the app's `env`
    /// verbatim, which is why the file is written owner-only, the same
    /// mode shep's own roll uses for the same reason.
    ///
    /// It serialises as `[origin]` with `[origin.env]` and the probe tables
    /// under it, at the END of the file whatever this field's position:
    /// this crate's `toml` hoists every scalar above every table. So the
    /// record an operator opens still starts with the lines they edit.
    #[serde(default)]
    pub origin: Option<AppConfig>,
    /// The sha built by `deploy <sheep> --stage` and not yet landed, or
    /// `None` when nothing is staged.
    ///
    /// Left out of the file while `None`, so a record that has never been
    /// staged still reads in a shep-deploy from before the field, which
    /// refuses unknown fields. Cleared when a deploy lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<String>,
}

impl State {
    /// Read and parse the `deploy.toml` at `path`, refusing a record whose
    /// values cannot work.
    ///
    /// The result holds the default `branch`, `watch` and `verify` until
    /// [`Self::adopt`] fills them in from the sheep's table.
    ///
    /// # Errors
    /// [`Error::Io`], naming `path`, if the file cannot be read - most
    /// often because this sheep is not a deploy target at all.
    /// [`Error::Config`] if it is not valid TOML, carries a key that moved to
    /// `[app.dogs.deploy]`, is missing a field that has no default, or fails
    /// [`Self::validate`].
    pub fn read(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(Error::at(path))?;
        let refuse = |source: &dyn core::fmt::Display| {
            Error::Config(format!("{}: {source}", path.display()))
        };
        let table: toml::Table = toml::from_str(&text).map_err(|source| refuse(&source))?;
        if let Some(key) = MOVED.iter().find(|key| table.contains_key(**key)) {
            return Err(refuse(&format!(
                "`{key}` moved to `[app.dogs.deploy]` in the sheep's settings. Set it there \
                 and remove it from this file"
            )));
        }
        let state: Self = table.try_into().map_err(|source| refuse(&source))?;
        state.validate(path)?;
        Ok(state)
    }

    /// Fills in the three settings this record no longer holds from the
    /// sheep's `[app.dogs.deploy]` table.
    ///
    /// # Errors
    /// [`Error::Config`], naming `sheep`, if the table sets no `branch`, or
    /// one that [`branch_refusal`] refuses.
    pub fn adopt(&mut self, sheep: &str, settings: &Settings) -> Result<(), Error> {
        let Some(branch) = &settings.branch else {
            return Err(Error::Config(format!(
                "{sheep}: `[app.dogs.deploy]` sets no `branch`, and a branch used to live in \
                 `deploy.toml`. Set the one this sheep follows"
            )));
        };
        if let Some(why) = branch_refusal(branch) {
            return Err(Error::Config(format!(
                "{sheep}: `branch` in `[app.dogs.deploy]` {why}"
            )));
        }
        self.branch.clone_from(branch);
        self.watch = settings.watch;
        self.verify = settings.verify;
        Ok(())
    }

    /// Refuses a record that parses and still cannot be acted on, naming the
    /// field and what is wrong with it.
    ///
    /// Every field here is handed to git or joined onto a path later, where
    /// a bad value fails in words that name the mechanism rather than the
    /// mistake. A `deployed` of `""` makes `Tree::release("")` the
    /// `releases/` directory itself, which exists, so a rollback would point
    /// `current` at it. A `remote` beginning with `-` is an option to git
    /// rather than a URL, and `--upload-pack=<command>` is one it runs.
    /// Measured 2026-09-03.
    ///
    /// The branch is not checked here: it is not in the file, and
    /// [`Self::adopt`] checks it with [`branch_refusal`].
    ///
    /// # Errors
    /// [`Error::Config`], naming `path` and the field, for: an empty
    /// `remote` or `checkout`; a `remote` beginning with `-` or carrying a
    /// character that would rewrite a log line; a relative `checkout`; a
    /// `deployed`, `failed` or `staged` that is not a full commit sha; `deployed` and
    /// `failed` naming the same sha; or exactly one of `origin_cwd` and
    /// `origin_script` set.
    pub(crate) fn validate(&self, path: &Path) -> Result<(), Error> {
        let refuse =
            |field: &str, why: &str| Error::Config(format!("{}: `{field}` {why}", path.display()));

        if let Some(why) = name_refusal(&self.remote) {
            return Err(refuse("remote", why));
        }

        if self.checkout.as_os_str().is_empty() {
            return Err(refuse("checkout", "is empty"));
        }
        if !self.checkout.is_absolute() {
            return Err(refuse(
                "checkout",
                "is not an absolute path, and a relative one would be resolved against \
                 wherever the dog happens to be running",
            ));
        }

        for (field, value) in [
            ("deployed", &self.deployed),
            ("failed", &self.failed),
            ("staged", &self.staged),
        ] {
            if let Some(sha) = value
                && !is_sha(sha)
            {
                return Err(refuse(
                    field,
                    "is not a full commit sha: 40 (or 64) hexadecimal characters, as `git \
                     rev-parse` prints one",
                ));
            }
        }
        if self.deployed.is_some() && self.deployed == self.failed {
            return Err(refuse(
                "failed",
                "names the sha `deployed` names. A release that is serving cannot also be the \
                 one being held; remove `failed`",
            ));
        }

        if self.origin.is_some() && (self.origin_cwd.is_none() || self.origin_script.is_none()) {
            return Err(refuse(
                "origin",
                "is set while origin_cwd or origin_script is missing. All three are written \
                 together at setup, and removal needs the pair to say where the sheep goes \
                 back to; set the pair or remove `origin`",
            ));
        }

        if self.origin_cwd.is_some() != self.origin_script.is_some() {
            let missing = if self.origin_cwd.is_none() {
                "origin_cwd"
            } else {
                "origin_script"
            };
            return Err(refuse(
                missing,
                "is missing while its partner is set. Both are written together at setup and \
                 both are needed to put the sheep back on removal; set both or neither",
            ));
        }

        Ok(())
    }

    /// Serialise to TOML and write to `path` atomically and durably.
    ///
    /// The write lands in a uniquely named staging file beside `path` first
    /// (shep-core's `atomic_file::create_staging_file`, created exclusively
    /// and owner-only at the `open` itself), is flushed to disk, then
    /// `rename(2)`d over `path`, and the directory is flushed after that. `rename(2)` within one directory is a single
    /// atomic syscall, so a process killed mid-write leaves either the old
    /// `deploy.toml` intact or the new one in full - never a truncated file.
    /// The two flushes extend that from a killed process to a lost power
    /// supply: without them the rename can reach the disk ahead of the bytes
    /// it names, which leaves a zero-length record. That matters here
    /// specifically because this file is the only record of how to restore
    /// the sheep once the dog is removed.
    ///
    /// A failed write or rename removes the staging file it leaves behind,
    /// so one failure does not sit next to the record forever, unread by
    /// anything. Exclusive creation with a name nothing else can predict is
    /// also what keeps a link planted at a staging name from being written
    /// through: the open never follows one, and a `rename` over `path`
    /// replaces a link there rather than writing through it.
    ///
    /// The file is created owner-only. Since 2026-09-04 it can carry the
    /// app's `env` verbatim in [`Self::origin`], and shep writes its own
    /// roll at the same mode for the same reason. A record left at a wider
    /// mode by an earlier version is tightened on the next write, because
    /// the rename replaces it with the fresh file.
    ///
    /// # Errors
    /// [`Error::Config`] if the record fails [`Self::validate`]: nothing is
    /// written. [`Error::Io`], naming whichever of `path`, the staging file
    /// or the directory the failing operation touched, and a directory that
    /// cannot be flushed is a failure: the rename has landed, and what did
    /// not happen is the durability this doc promises. This includes the
    /// (practically
    /// unreachable, but not impossible) case of a `checkout` or
    /// `origin_cwd` containing non-UTF-8 bytes, which TOML cannot represent
    /// as a string; that failure is reported against `path` before any
    /// write is attempted, so it never touches the temp file at all.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        // Validated on the way out as well as on the way in, because every
        // writer is inside this crate: a record this crate persists and then
        // refuses to read on every later command is a target nothing can
        // touch, and the mistake is cheaper at the moment it is made.
        self.validate(path)?;
        let text = toml::to_string_pretty(self).map_err(|source| Error::Io {
            path: path.to_owned(),
            source: io::Error::other(source),
        })?;

        // A unique name, because `set_watch` writes without the tree lock
        // (see its doc for why) and two processes staging into one fixed
        // name would truncate each other's file: the second rename then
        // fails, and a rename landing between the other's truncate and
        // write exposes an empty record. Created exclusively, so a link
        // planted at the name is never followed, and owner-only at the
        // open, so the `env` inside is never briefly wider.
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("deploy.toml");
        let mut staged =
            create_staging_file(dir, &format!("{name}."), ".tmp").map_err(Error::at(dir))?;

        let written = staged
            .as_file_mut()
            .write_all(text.as_bytes())
            .and_then(|()| staged.as_file().sync_all());
        if let Err(source) = written {
            // Dropping `staged` removes it.
            return Err(Error::Io {
                path: staged.path().to_owned(),
                source,
            });
        }

        // `persist` is the rename; a refusal hands the staging file back,
        // and dropping it removes it.
        staged.persist(path).map_err(|failed| Error::Io {
            path: path.to_owned(),
            source: failed.error,
        })?;

        // The directory entry, so the rename itself survives a power loss.
        // Not best effort: the helper already forgives the one answer
        // (`EINVAL`) that means "no such step here", so what reaches this
        // is a flush that did not happen.
        sync_dir(dir).map_err(Error::at(dir))
    }
}

/// Why `name`, a remote or a branch, cannot be handed to git, or `None` when
/// it can.
fn name_refusal(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("is empty")
    } else if name.starts_with('-') {
        Some("begins with `-`, which git would read as an option rather than a name")
    } else if name.chars().any(crate::shared::forges_a_line) {
        Some("carries a control or invisible character that would rewrite a log line")
    } else {
        None
    }
}

/// Why `branch`, a branch name or a full ref, cannot be followed, or `None`
/// when it can.
///
/// An empty one reaches `git rev-parse --verify refs/heads/` and fails as
/// "needed a single revision". One beginning `refs/` is checked against git's
/// own ref rules by [`ref_refusal`].
#[must_use]
pub(crate) fn branch_refusal(branch: &str) -> Option<&'static str> {
    name_refusal(branch).or_else(|| {
        branch
            .starts_with("refs/")
            .then(|| ref_refusal(branch))
            .flatten()
    })
}

/// Why `tracked`, a value beginning `refs/`, cannot be fetched as the ref it
/// names, or `None` when it can.
///
/// Git's own ref rules, plus `:` and `*`, which git also refuses and which
/// would turn the `+<ref>:<ref>` refspec built from this value into a
/// different one.
fn ref_refusal(tracked: &str) -> Option<&'static str> {
    const CARRIES: [(&str, &str); 11] = [
        (" ", "carries a space, which git refuses in a ref name"),
        (
            ":",
            "carries `:`, which would split the refspec into a source and a destination",
        ),
        (
            "*",
            "carries `*`, which would turn the refspec into a pattern",
        ),
        ("?", "carries `?`, which git refuses in a ref name"),
        ("[", "carries `[`, which git refuses in a ref name"),
        ("\\", "carries `\\`, which git refuses in a ref name"),
        ("^", "carries `^`, which git refuses in a ref name"),
        ("~", "carries `~`, which git refuses in a ref name"),
        ("..", "carries `..`, which git refuses in a ref name"),
        ("@{", "carries `@{`, which git refuses in a ref name"),
        (
            "//",
            "carries `//`, an empty path component, which git refuses in a ref name",
        ),
    ];
    if let Some((_, why)) = CARRIES.iter().find(|(needle, _)| tracked.contains(needle)) {
        return Some(why);
    }
    if tracked.ends_with('/') {
        return Some("ends with `/`, which git refuses in a ref name");
    }
    if tracked.ends_with('.') {
        return Some("ends with `.`, which git refuses in a ref name");
    }
    for component in tracked.split('/') {
        if component.ends_with(".lock") {
            return Some("has a path component ending `.lock`, which git refuses in a ref name");
        }
        if component.starts_with('.') {
            return Some("has a path component beginning `.`, which git refuses in a ref name");
        }
    }
    None
}

/// Whether `text` is a full git object id: 40 hexadecimal characters for
/// SHA-1, 64 for SHA-256.
///
/// The one rule for what a release is named by, shared with
/// `crate::retention` so the two cannot disagree about which directories
/// under `releases/` are releases.
#[must_use]
pub fn is_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, SHA};

    /// fails if a key `deploy.toml` does not know is silently dropped.
    ///
    /// An older shep-deploy reads a record a newer one wrote, and a key it
    /// does not know is a field the newer one relies on. Without this a
    /// `stagd` parses fine, leaves nothing staged, and the next promote
    /// refuses with nothing anywhere indicating the field was ignored.
    #[test]
    fn an_unknown_key_in_the_record_is_refused_and_named() {
        // `checkout` is present, and load-bearing: it has no default, so
        // leaving it out made the document invalid twice over and the test
        // passed on whichever error serde reported first. It still caught the
        // guard being removed, but failed with "TOML parse error at line 1"
        // instead of naming the typo, which sends the next reader to the wrong
        // place.
        let toml = r#"
remote = "https://example.invalid/repo.git"
checkout = "/srv/web"
stagd = "x"
"#;
        let err = toml::from_str::<State>(toml).expect_err("a typo must not parse");
        assert!(
            format!("{err}").contains("stagd"),
            "the refusal must name the key: {err}"
        );
    }

    /// fails if a record that still carries a key that moved to the sheep's
    /// table is read, or is refused without saying where the key went. The
    /// copy in the file would be ignored by every deploy, so reading it is
    /// the silent version of the mistake.
    #[test]
    fn a_record_carrying_a_moved_key_is_refused_naming_the_key_and_its_new_home() {
        for (key, value) in [
            ("verify", "\"alive\""),
            ("watch", "\"manual\""),
            ("branch", "\"main\""),
        ] {
            let text = format!(
                "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n{key} = {value}\n"
            );

            let err = read_text(&text).expect_err(&format!("must refuse: {key}"));

            assert!(matches!(err, Error::Config(_)), "{key}: {err}");
            let shown = err.to_string();
            assert!(shown.contains(&format!("`{key}`")), "{shown}");
            assert!(shown.contains("[app.dogs.deploy]"), "{shown}");
        }
    }

    /// fails if state does not survive a write-then-read, or if the write
    /// puts a setting in the file. This file is the only record of how a
    /// sheep ran BEFORE the dog took over, so losing a field means removal
    /// cannot restore the sheep and the operator is left with an app running
    /// from a path under $SHEP_HOME they have no reason to know about.
    ///
    /// The settings are non-defaults, so a key written or an adoption that
    /// drops one shows.
    #[test]
    fn state_round_trips_through_toml_without_the_settings() {
        let settings = Settings {
            branch: Some("release".to_owned()),
            watch: Watch::Manual,
            verify: Verify::Alive,
            ..Settings::default()
        };
        let mut original = State {
            remote: "https://github.com/WatWowMap/ReactMap".into(),
            deployed: Some("a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2".into()),
            origin_cwd: Some(PathBuf::from("/srv/reactmap")),
            origin_script: Some("bun .".into()),
            checkout: PathBuf::from("/srv/reactmap"),
            ..fixtures::state()
        };
        original.adopt("web", &settings).expect("adopts");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");

        original.write(&path).expect("writes");
        let text = fs::read_to_string(&path).expect("record");
        let mut back = State::read(&path).expect("reads");

        for key in ["branch", "watch", "verify", "release", "alive", "manual"] {
            assert!(!text.contains(key), "{key} is in the file:\n{text}");
        }
        assert_eq!(back.branch, "", "nothing is adopted by reading");
        back.adopt("web", &settings).expect("adopts");
        assert_eq!(back, original);
    }

    /// Whether any `*.tmp` sits in `dir`: the staging file a write leaves
    /// behind when it does not finish.
    fn any_tmp(dir: &Path) -> bool {
        fs::read_dir(dir)
            .expect("list")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
    }

    /// Writes `text` as a record and reads it back through `State::read`,
    /// so the validation runs the way it does in production.
    fn read_text(text: &str) -> Result<State, Error> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        fs::write(&path, text).expect("write record");
        State::read(&path)
    }

    /// fails if a record that parses is acted on when its values cannot
    /// work. Each of these used to reach git, or a path join, and fail
    /// there in words about the mechanism: an empty `branch` as "needed a
    /// single revision", `deployed = ""` as a rollback onto `releases/`
    /// itself, a `remote` beginning with `-` as whatever git made of the
    /// option. The refusal has to name the field an operator typed.
    #[test]
    fn a_record_whose_values_cannot_work_is_refused_by_field() {
        let base = |extra: &str| {
            format!("remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n{extra}")
        };
        let cases = [
            ("remote", "remote = \"\"\ncheckout = \"/srv/x\"".to_owned()),
            (
                "remote",
                "remote = \"--upload-pack=x\"\ncheckout = \"/srv/x\"".to_owned(),
            ),
            (
                "checkout",
                "remote = \"https://example.com/x\"\ncheckout = \"\"".to_owned(),
            ),
            (
                "checkout",
                "remote = \"https://example.com/x\"\ncheckout = \"srv/x\"".to_owned(),
            ),
            ("deployed", base("deployed = \"\"")),
            ("deployed", base("deployed = \"main\"")),
            ("deployed", base("deployed = \"abc123\"")),
            ("failed", base("failed = \"..\"")),
            (
                "failed",
                base(&format!("deployed = \"{SHA}\"\nfailed = \"{SHA}\"")),
            ),
            ("origin_script", base("origin_cwd = \"/srv/x\"")),
            ("origin_cwd", base("origin_script = \"bun .\"")),
        ];
        for (field, text) in cases {
            let err = read_text(&text).expect_err(&format!("must refuse: {text}"));
            assert!(matches!(err, Error::Config(_)), "{text}: {err}");
            assert!(
                err.to_string().contains(&format!("`{field}`")),
                "must name `{field}`: {err}"
            );
        }
    }

    /// Adopts `branch` as the table's, into a fresh record.
    fn adopt_branch(branch: Option<&str>) -> Result<State, Error> {
        let mut state = fixtures::state();
        state.adopt(
            "web",
            &Settings {
                branch: branch.map(str::to_owned),
                ..Settings::default()
            },
        )?;
        Ok(state)
    }

    /// fails if a branch that cannot work is adopted, or is refused in words
    /// about git's mechanism. Each of these used to reach git: an empty one as
    /// "needed a single revision", one beginning `-` as whatever git made of
    /// the option. The refusal has to name the table the operator typed it in.
    #[test]
    fn a_branch_that_cannot_work_is_refused_naming_the_table() {
        for branch in ["", "  ", "-x", "ma\nin"] {
            let err = adopt_branch(Some(branch)).expect_err(&format!("must refuse: {branch:?}"));
            assert!(matches!(err, Error::Config(_)), "{branch:?}: {err}");
            let shown = err.to_string();
            assert!(shown.contains("`branch`"), "{branch:?}: {shown}");
            assert!(shown.contains("[app.dogs.deploy]"), "{branch:?}: {shown}");
        }
    }

    /// fails if a table with no `branch` is adopted as one that follows
    /// nothing, or as `main`. A guess would deploy a branch nobody chose.
    #[test]
    fn a_table_with_no_branch_is_refused_naming_the_key() {
        let err = adopt_branch(None).expect_err("no branch to follow");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("sets no `branch`"), "{shown}");
        assert!(shown.contains("[app.dogs.deploy]"), "{shown}");
    }

    /// fails if a `refs/` value that git would refuse as a ref name, or that
    /// would change what a refspec means, reaches the fetch. `:` splits a
    /// refspec into source and destination and `*` makes it a pattern, so
    /// either would make the dog fetch something other than the ref named.
    #[test]
    fn a_ref_with_a_colon_or_star_is_refused() {
        for value in [
            "refs/pull/1/head:refs/heads/main",
            "refs/pull/*/head",
            "refs/pull/1 /head",
            "refs/pull/?/head",
            "refs/pull/[1]/head",
            "refs/pull\\1/head",
            "refs/pull/1^/head",
            "refs/pull/1~/head",
            "refs/pull/../head",
            "refs/pull/@{1}/head",
            "refs/pull/1/",
            "refs/pull/1.lock",
            "refs/pull/1.lock/head",
            "refs/pull/.hidden/head",
            "refs/pull/1/head.",
            "refs/pull//1/head",
        ] {
            let err = adopt_branch(Some(value)).expect_err(&format!("must refuse: {value}"));
            assert!(matches!(err, Error::Config(_)), "{value}: {err}");
            assert!(err.to_string().contains("`branch`"), "{value}: {err}");
        }
    }

    /// fails if a ref a forge really publishes is refused.
    #[test]
    fn a_well_formed_pull_ref_is_accepted() {
        for value in [
            "refs/pull/1/head",
            "refs/pull/1234/merge",
            "refs/tags/v1.2.3",
        ] {
            let state = adopt_branch(Some(value)).expect(value);
            assert_eq!(state.branch, value);
        }
    }

    /// fails if a record with every value in shape is refused, or if a
    /// zero-length one - what a crash mid-write used to be able to leave -
    /// is read as anything but a named failure.
    #[test]
    fn a_sound_record_reads_and_an_empty_one_is_refused() {
        let text = format!(
            "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n\
             deployed = \"{SHA}\"\norigin_cwd = \"/srv/x\"\norigin_script = \"bun .\"\n"
        );
        let state = read_text(&text).expect("a sound record");
        assert_eq!(state.deployed.as_deref(), Some(SHA));

        let err = read_text("").expect_err("nothing to read");
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(err.to_string().contains("deploy.toml"), "{err}");
    }

    /// fails if the pre-adoption app definition does not survive a
    /// write-then-read, `env` and probe included, or if a record written
    /// before the field existed stops reading. The first is what removal
    /// restores from; the second is every record on disk today.
    #[test]
    fn the_origin_round_trips_and_an_older_record_still_reads() {
        let origin: AppConfig = toml::from_str(
            "name = \"web\"\nscript = \"server.js\"\ninstances = 2\n[env]\nTOKEN = \"s3cret\"\n\
             [readiness_probe]\nkind = \"http\"\ntarget = \"http://127.0.0.1:3000/health\"\n",
        )
        .expect("an app");
        let mut with = sample(None);
        with.origin = Some(origin.clone());
        with.origin_cwd = Some(PathBuf::from("/srv/web"));
        with.origin_script = Some("server.js".to_owned());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        with.write(&path).expect("writes");
        let back = State::read(&path).expect("reads");
        assert_eq!(back.origin.as_ref(), Some(&origin));

        let older = read_text("remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n")
            .expect("a record from before the field");
        assert_eq!(older.origin, None);
    }

    /// fails if a `State` printed with `{:?}` shows an `env` value. The
    /// record carries the app's `env` verbatim in `origin`, and the derive
    /// is safe only because `AppConfig`'s own `Debug` prints the map as a
    /// count; this pins that exact shape so a change to either side shows
    /// up here rather than in a log.
    #[test]
    fn debug_output_redacts_the_origins_env() {
        let origin: AppConfig = toml::from_str(
            "name = \"web\"\nscript = \"server.js\"\n[env]\nTOKEN = \"hunter2-distinctive\"\n",
        )
        .expect("an app");
        let state = State {
            origin: Some(origin),
            ..fixtures::state()
        };

        let shown = format!("{state:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("TOKEN"), "{shown}");
        assert!(shown.contains("env: <1 vars>"), "{shown}");
    }

    /// fails if a record carrying `origin` without the legacy pair reads.
    /// Removal puts the sheep back through both, and a record with one and
    /// not the other was accepted and then skipped as a dog-bootstrapped
    /// sheep, leaving it under the tree with its origin unread.
    #[test]
    fn an_origin_without_the_legacy_pair_is_refused_by_name() {
        let origin: AppConfig =
            toml::from_str("name = \"web\"\nscript = \"server.js\"\ncwd = \"/srv/web\"\n")
                .expect("an app");
        let state = State {
            origin: Some(origin),
            origin_cwd: None,
            origin_script: None,
            ..fixtures::state()
        };

        let err = state
            .validate(Path::new("/x/deploy.toml"))
            .expect_err("origin without the pair");
        let shown = err.to_string();
        assert!(shown.contains("origin"), "{shown}");
        assert!(shown.contains("origin_cwd"), "{shown}");
    }

    /// fails if a write follows a link planted where the record goes. The
    /// rename replaces the link with the real file; the file the link named
    /// is never opened, let alone written with the record's `env`.
    #[test]
    fn a_link_at_the_record_path_is_replaced_not_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        fs::write(&victim, "untouched").expect("victim");
        let path = dir.path().join("deploy.toml");
        std::os::unix::fs::symlink(&victim, &path).expect("a planted link");

        sample(None).write(&path).expect("writes");

        assert_eq!(fs::read_to_string(&victim).expect("victim"), "untouched");
        assert!(
            !fs::symlink_metadata(&path)
                .expect("record")
                .file_type()
                .is_symlink(),
            "the record is a real file now"
        );
        assert!(State::read(&path).is_ok());
    }

    /// fails if the record is readable by anyone but its owner. It carries
    /// the app's `env` verbatim once an origin is recorded, and shep's own
    /// roll is owner-only for the same reason.
    #[test]
    fn the_record_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        use shep_client::shep_core::atomic_file::OWNER_ONLY_FILE_MODE;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        sample(None).write(&path).expect("writes");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, OWNER_ONLY_FILE_MODE);
    }

    /// fails if a rename that cannot land leaves its temporary file behind.
    /// Nothing reads `deploy.toml.tmp`, so a stale one would sit beside the
    /// record forever, a full copy of it that no listing ever mentions.
    #[test]
    fn a_failed_write_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the record should go, so the rename fails.
        let path = dir.path().join("deploy.toml");
        fs::create_dir_all(&path).expect("something in the way");

        sample(None)
            .write(&path)
            .expect_err("cannot rename onto a directory");

        assert!(
            !any_tmp(dir.path()),
            "the temporary file must be removed with the failure"
        );
    }

    /// fails if reading a `deploy.toml` that is not there produces
    /// anything other than a named I/O failure. An operator asking to
    /// deploy a sheep that was never set up as a target meets this path,
    /// and the path it names is the only clue about what is missing.
    #[test]
    fn reading_a_missing_state_file_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        let err = State::read(&path).expect_err("nothing to read");
        assert!(err.to_string().contains("deploy.toml"));
    }

    /// fails if a table that sets neither `watch` nor `verify` adopts as
    /// anything but auto and probed. A table written before these keys
    /// moved there must keep being watched rather than silently stop
    /// deploying, and an absent `verify` must mean "wait for the readiness
    /// probe", never "deploy without checking".
    #[test]
    fn a_table_that_sets_neither_adopts_auto_and_probed() {
        let mut state = State {
            watch: Watch::Manual,
            verify: Verify::Alive,
            ..fixtures::state()
        };

        state
            .adopt(
                "web",
                &Settings {
                    branch: Some("main".to_owned()),
                    ..Settings::default()
                },
            )
            .expect("adopts");

        assert_eq!(state.watch, Watch::Auto);
        assert_eq!(state.verify, Verify::Probed);
    }

    /// fails if a staged sha is lost between a write and a read, or if a
    /// record with nothing staged grows a `staged` line. The second is what
    /// keeps an older shep-deploy, which refuses unknown fields, able to
    /// read every record that has never been staged.
    #[test]
    fn a_staged_sha_round_trips_and_an_empty_one_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");

        let staged = State {
            staged: Some(fixtures::OTHER_SHA.to_owned()),
            ..fixtures::state()
        };
        staged.write(&path).expect("writes");
        assert_eq!(State::read(&path).expect("reads").staged, staged.staged);

        fixtures::state().write(&path).expect("writes");
        let text = fs::read_to_string(&path).expect("record");
        assert!(!text.contains("staged"), "{text}");
    }

    /// fails if a hand-edited `staged` that names no commit is accepted. It
    /// is joined onto `releases/` and handed to the next deploy as the
    /// release to land, so the refusal has to name the field.
    #[test]
    fn a_staged_value_that_is_not_a_sha_is_refused_by_field() {
        let base = "remote = \"https://example.com/x\"\ncheckout = \"/srv/x\"\n";
        for staged in ["\"\"", "\"main\"", "\"abc123\"", "\"..\""] {
            let text = format!("{base}staged = {staged}");
            let err = read_text(&text).expect_err(&format!("must refuse: {text}"));
            assert!(matches!(err, Error::Config(_)), "{text}: {err}");
            assert!(err.to_string().contains("`staged`"), "{text}: {err}");
        }
    }

    /// A minimal, valid `State` for the write tests below, where the exact
    /// values don't matter - only that they change between the two writes.
    fn sample(deployed: Option<&str>) -> State {
        State {
            deployed: deployed.map(str::to_owned),
            ..fixtures::state()
        }
    }

    /// Pins that `write` works and that a completed write leaves no `.tmp`
    /// file behind - it does NOT, and cannot, prove atomicity. A
    /// single-threaded test has no way to observe a process crashing
    /// mid-write, so the atomic-rename claim rests on the implementation
    /// shape (write to a sibling temp file, then one `rename(2)` within the
    /// same directory) rather than on anything this test checks. Two writes
    /// are run specifically to prove the *second* write's tmp file is also
    /// cleaned up, not just a first write starting from nothing.
    #[test]
    fn write_survives_a_second_write_and_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deploy.toml");
        let tmp = dir.path();

        let first = sample(None);
        first.write(&path).expect("first write");
        assert!(path.exists(), "the state file must exist after a write");
        assert!(
            !any_tmp(tmp),
            "a completed write must not leave a .tmp file"
        );

        let second = sample(Some(fixtures::OTHER_SHA));
        second.write(&path).expect("second write");
        assert!(
            !any_tmp(tmp),
            "a second, overwriting write must also leave no .tmp file"
        );

        assert_eq!(State::read(&path).expect("reads").deployed, second.deployed);
    }
}
