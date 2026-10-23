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
mod tests;
