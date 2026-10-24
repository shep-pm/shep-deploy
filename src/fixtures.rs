//! Test helpers shared by every module's own `mod tests`.
//!
//! Compiled only under `cfg(test)` and declared from `main.rs`, which is the
//! only way a binary crate with no library target can share one of these.
//!
//! # Why this exists
//!
//! Before it, `run_git` was copy-pasted verbatim into six modules, `head_of`
//! into three, `fixture_release` into two, and `TEST_BUDGET` into four. That
//! is not a tidiness complaint: the copies had already drifted. The one in
//! `tests/integration/` prints the directory when git fails and the six in
//! `src/` do not, so an improvement landed once and six fixtures kept the
//! worse message. `build.rs`'s copy of `fixture_release` even carried a doc
//! comment naming the duplication and leaving it there.
//!
//! Four more kinds went the same way afterwards, all found in one review pass.
//! A `DogConfig` literal was written out eight times, differing in nothing but
//! `interval`. A `State` literal was written out eleven, and its shas were
//! whatever length the module happened to type, which stopped being free the
//! day [`State::read`] started refusing anything but a full forty. The "git
//! init, set an identity, write a file, commit" opening was written seven
//! times, and one of the seven had already been fixed on its own after
//! failing on CI for want of a `user.email`, with the other six left alone.
//! And fourteen [`Daemon`](crate::daemon::Daemon) doubles hand-wrote 78
//! `unimplemented!()` methods between them, which buried the one or two
//! methods each double really answers.
//!
//! `tests/integration/` is a separate compilation target and cannot reach
//! this module, so it keeps its own copy. One copy across a target boundary is
//! the cost of the boundary; six inside one target was not.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use shep_client::shep_core::config::{AppConfig, DogTable};
use tempfile::TempDir;

use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::settings::{self, Settings};
use crate::state::{State, Verify, Watch};

/// A full commit sha, as `git rev-parse` prints one.
///
/// Spelled out rather than shortened because [`State::read`] refuses a
/// `deployed` or `failed` that is not forty hex characters, so a fixture
/// carrying `"old"` parses in one test and is rejected in the next.
pub const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A second full sha, for a test that needs two that differ.
pub const OTHER_SHA: &str = "a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2c3a1b2";

/// A git budget the test tier can never legitimately hit.
///
/// Every fixture here drives a local repository, so anything slower than a
/// minute is a hang worth failing on rather than waiting out. Distinct from
/// the production default, which is generous on purpose because a cold clone
/// of a real repository legitimately runs minutes.
pub const TEST_BUDGET: Duration = Duration::from_secs(60);

/// The dog config every test that is not about a config value runs on.
///
/// The timeouts are a minute rather than the production five minutes and hour,
/// for the same reason as [`TEST_BUDGET`]: nothing here talks to a network or
/// runs a real build, so anything slower is a hang.
///
/// A test that cares about one field says so by name and takes the rest from
/// here:
///
/// ```ignore
/// DogConfig { interval: Duration::from_secs(150), ..fixtures::dog_config() }
/// ```
pub fn dog_config() -> DogConfig {
    DogConfig {
        interval: Duration::from_secs(30),
        retention: 5,
        git_timeout: Duration::from_secs(60),
        build_timeout: Duration::from_secs(60),
        passthrough: Vec::new(),
    }
}

/// A target's record with every field set to something [`State::read`] will
/// accept: a remote, `main`, an absolute checkout, nothing deployed, nothing
/// failed, and no origin.
///
/// A test names only the fields it is about and takes the rest from here:
///
/// ```ignore
/// State { deployed: Some(fixtures::SHA.to_owned()), watch, ..fixtures::state() }
/// ```
///
/// `origin_cwd` and `origin_script` are both `None` because `read` refuses a
/// record with one and not the other, so a fixture that set one alone would be
/// a record no opt-in ever writes.
pub fn state() -> State {
    State {
        remote: "https://example.com/x".to_owned(),
        branch: "main".to_owned(),
        deployed: None,
        failed: None,
        verify: Verify::default(),
        watch: Watch::default(),
        origin_cwd: None,
        origin_script: None,
        checkout: PathBuf::from("/srv/x"),
        origin: None,
        staged: None,
    }
}

/// Runs `git <args>` in `dir`, panicking with the command AND the directory if
/// it fails.
///
/// The directory is in the message deliberately. Every fixture here works in a
/// tempdir with a generated name, so a bare "git \[...\] failed" leaves you
/// guessing which of several repositories a test built was the broken one.
///
/// # Panics
/// If git cannot be spawned, or exits non-zero.
#[track_caller]
pub fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// The sha `HEAD` resolves to in `dir`.
///
/// # Panics
/// If git cannot be spawned, or prints something that is not UTF-8.
#[track_caller]
pub fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    // Checked, because the failure is silent otherwise: a repository with no
    // commits gives an empty stdout and a status nobody reads, so the caller
    // gets `""` and asserts against it. That exact shape wasted two attempts
    // on a fixture earlier in this review.
    assert!(
        out.status.success(),
        "git rev-parse HEAD failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8(out.stdout)
        .expect("utf-8 sha")
        .trim()
        .to_owned()
}

/// Writes `files` as `(path, contents)` under `dir`, creating any directory a
/// path names on the way.
///
/// # Panics
/// If a directory or any file cannot be written.
#[track_caller]
pub fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent");
        }
        std::fs::write(&full, contents).expect("write fixture file");
    }
}

/// A throwaway directory standing in for a checked-out release, holding
/// `files` as `(name, contents)`.
///
/// # Panics
/// If the directory or any file cannot be written.
#[track_caller]
pub fn fixture_release(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    write_files(dir.path(), files);
    dir
}

/// A throwaway git repository on `main` with a commit identity set, and
/// nothing committed yet.
///
/// The branch is named explicitly because `init.defaultBranch` is a user
/// setting no test can assume, and the identity is set because a commit
/// without one fails. A developer's machine has a global `user.email` and a
/// CI runner does not, so leaving the identity out makes a fixture pass
/// locally and fail only on CI, which is where this repository's very first
/// push failed.
///
/// # Panics
/// If the directory cannot be created, or git fails.
#[track_caller]
pub fn empty_checkout() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "test"]);
    dir
}

/// Writes `files` into the repository at `dir` and commits everything staged
/// by `git add .` as one commit named `message`.
///
/// `git add .` rather than adding the paths by name, so an entry matched by a
/// `.gitignore` stays untracked instead of being forced into the commit.
/// Several fixtures below want exactly that split: something tracked,
/// something ignored and present.
///
/// # Panics
/// If a file cannot be written, or git fails.
#[track_caller]
pub fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
    write_files(dir, files);
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

/// An [`empty_checkout`] with `files` written and committed as its first and
/// only commit.
///
/// # Panics
/// If the directory cannot be created, a file cannot be written, or git
/// fails. Committing nothing fails, so `files` must name at least one file
/// that is not ignored.
#[track_caller]
pub fn checkout(files: &[(&str, &str)]) -> TempDir {
    let dir = empty_checkout();
    commit(dir.path(), files, "first");
    dir
}

/// Fills in the [`Daemon`](crate::daemon::Daemon) methods a test double never
/// has called on it, each panicking with its own name.
///
/// Every double in this crate implements one or two methods and stands the
/// other seven or eight up as `unimplemented!()`, which was 78 lines of
/// boilerplate across fourteen doubles and, worse, hid which methods a
/// double actually answers behind a wall of identical ones. A double that
/// answers every method the same way, a daemon that cannot be reached, say,
/// names the answer once with the `answering` form.
///
/// ```ignore
/// impl Daemon for Recording {
///     async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> { .. }
///
///     fixtures::daemon_methods!(unimplemented;
///         dog_config, list_flock, describe, start, delete, reload, restart, save_roll, sheep_config,
///     );
/// }
/// ```
///
/// The signatures are spelled out arm by arm rather than generated, because a
/// macro cannot see a trait's own declarations. A method added to `Daemon`
/// therefore needs an arm here too, and a call site naming a method with no
/// arm fails to compile rather than silently expanding to nothing.
macro_rules! daemon_methods {
    (@method dog_config, $body:expr) => {
        async fn dog_config(&self, _name: &str) -> Result<String, crate::error::Error> {
            $body
        }
    };
    (@method list_flock, $body:expr) => {
        async fn list_flock(
            &self,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, crate::error::Error>
        {
            $body
        }
    };
    (@method describe, $body:expr) => {
        async fn describe(
            &self,
            _sheep: &str,
        ) -> Result<Vec<shep_client::shep_core::protocol::ProcessInfo>, crate::error::Error>
        {
            $body
        }
    };
    (@method start, $body:expr) => {
        async fn start(
            &self,
            _apps: Vec<shep_client::shep_core::config::AppConfig>,
        ) -> Result<Vec<u32>, crate::error::Error> {
            $body
        }
    };
    (@method delete, $body:expr) => {
        async fn delete(&self, _id: u32) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method reload, $body:expr) => {
        async fn reload(&self, _sheep: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method restart, $body:expr) => {
        async fn restart(&self, _sheep: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method save_roll, $body:expr) => {
        async fn save_roll(&self) -> Result<std::path::PathBuf, crate::error::Error> {
            $body
        }
    };
    (@method sheep_config, $body:expr) => {
        async fn sheep_config(
            &self,
            _sheep: &str,
        ) -> Result<shep_client::shep_core::config::AppConfig, crate::error::Error> {
            $body
        }
    };
    (@method set_sheep_dog_settings, $body:expr) => {
        async fn set_sheep_dog_settings(
            &self,
            _sheep: &str,
            _dog: &str,
            _table: shep_client::shep_core::config::DogTable,
        ) -> Result<(), crate::error::Error> {
            $body
        }
    };
    (@method set_smit, $body:expr) => {
        async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), crate::error::Error> {
            $body
        }
    };
    // The methods a double never expects to be asked.
    (unimplemented; $($method:ident),* $(,)?) => {
        $(crate::fixtures::daemon_methods!(@method $method, unimplemented!(stringify!($method)));)*
    };
    // The methods a double answers the same way every time, `body` being
    // that answer: a daemon that cannot be reached, say.
    (answering $body:expr; $($method:ident),* $(,)?) => {
        $(crate::fixtures::daemon_methods!(@method $method, $body);)*
    };
}

pub(crate) use daemon_methods;

/// A sheep named `sheep` as the shepherd would hold it: a script and no
/// `[app.dogs.deploy]` table.
///
/// # Panics
/// Never in practice: the JSON is written here.
#[track_caller]
pub fn app_named(sheep: &str) -> AppConfig {
    serde_json::from_value(serde_json::json!({ "name": sheep, "script": "./run.sh" }))
        .expect("an app")
}

/// [`app_named`] carrying `settings` as its `[app.dogs.deploy]` table, in TOML.
///
/// # Panics
/// If `settings` is not a TOML table.
#[track_caller]
pub fn app_with_settings(sheep: &str, settings: &str) -> AppConfig {
    let table: serde_json::Value = toml::from_str(settings).expect("settings are TOML");
    serde_json::from_value(serde_json::json!({
        "name": sheep,
        "script": "./run.sh",
        "dogs": { "deploy": table },
    }))
    .expect("an app with settings")
}

/// [`app_with_settings`] with an `http` readiness probe, so `Verify::Probed`
/// has a gate to accept.
///
/// The probe's target is never contacted: no double here runs a real
/// shepherd.
///
/// # Panics
/// If `settings` is not a TOML table.
#[track_caller]
pub fn gated_app(sheep: &str, settings: &str) -> AppConfig {
    let table: serde_json::Value = toml::from_str(settings).expect("settings are TOML");
    serde_json::from_value(serde_json::json!({
        "name": sheep,
        "script": "./run.sh",
        "readiness_probe": { "kind": "http", "target": "http://127.0.0.1:1/health" },
        "dogs": { "deploy": table },
    }))
    .expect("a gated app")
}

/// One sheep's config as a shepherd holds it, and a [`Daemon`] that answers
/// `sheep_config` from it and applies `set_sheep_dog_settings` to it.
///
/// What a test reads back to see that a write reached the sheep's table, and
/// what a test edits between two ticks to move a setting under a running dog.
/// Every other method is unimplemented, so a double that needs more embeds
/// one and delegates these two.
#[derive(Debug)]
pub struct Sheep {
    app: RefCell<AppConfig>,
}

impl Sheep {
    /// `sheep` with `settings` as its `[app.dogs.deploy]` table, in TOML.
    ///
    /// # Panics
    /// If `settings` is not a TOML table.
    #[track_caller]
    pub fn of(sheep: &str, settings: &str) -> Self {
        Self {
            app: RefCell::new(app_with_settings(sheep, settings)),
        }
    }

    /// The sheep as `app`, whatever its table.
    pub fn holding(app: AppConfig) -> Self {
        Self {
            app: RefCell::new(app),
        }
    }

    /// The config as the shepherd holds it now.
    pub fn app(&self) -> AppConfig {
        self.app.borrow().clone()
    }

    /// The `[app.dogs.deploy]` table as the shepherd holds it now.
    ///
    /// # Panics
    /// If the table no longer parses as [`Settings`].
    #[track_caller]
    pub fn settings(&self) -> Settings {
        settings::from_app("web", &self.app.borrow()).expect("the table parses")
    }

    /// Sets `key` in the table to `value`, as an operator's pane would,
    /// leaving every other key.
    ///
    /// # Panics
    /// If the table cannot be edited.
    #[track_caller]
    pub fn set(&self, key: &str, value: &str) {
        let mut app = self.app.borrow_mut();
        let mut table = app
            .dogs
            .get("deploy")
            .map(|held| held.as_map().clone())
            .unwrap_or_default();
        table.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
        app.dogs.insert("deploy".to_owned(), DogTable::from(table));
    }

    /// The `sheep_config` answer.
    pub fn answer(&self) -> Result<AppConfig, Error> {
        Ok(self.app())
    }

    /// The `set_sheep_dog_settings` answer: replaces `dog`'s whole table, as
    /// the shepherd does.
    pub fn replace(&self, dog: &str, table: DogTable) -> Result<(), Error> {
        self.app.borrow_mut().dogs.insert(dog.to_owned(), table);
        Ok(())
    }
}

impl Daemon for Sheep {
    async fn sheep_config(&self, _sheep: &str) -> Result<AppConfig, Error> {
        self.answer()
    }

    async fn set_sheep_dog_settings(
        &self,
        _sheep: &str,
        dog: &str,
        table: DogTable,
    ) -> Result<(), Error> {
        self.replace(dog, table)
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, describe, start, delete, reload, restart, save_roll, set_smit,
    );
}

/// A bare throwaway directory, for tests that need a path rather than a
/// populated release.
///
/// # Panics
/// If the directory cannot be created.
#[track_caller]
pub fn tempdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// A shepherd that answers [`Daemon::dog_config`](crate::daemon::Daemon)
/// from a list of sections, in order, and counts the asks.
///
/// The last entry repeats once the list runs out, so a test writes only the
/// changes it is about and nothing to pad a loop out with. Counting the
/// asks is how a test counts ticks: the poll loop reads the section exactly
/// once per tick, before anything else the tick does, so a home with no
/// targets in it at all is enough to drive one and no repository has to be
/// built.
///
/// Every other method is unimplemented. A test that wants a deploy to
/// really run wants one of `crate::poll`'s own doubles instead.
///
/// `Debug` is hand-written to keep the sections out of it (IR-41). They
/// are `[dog.<name>]` section bodies, and a real one routinely carries
/// webhook credentials for other dogs, so a fixture that printed them
/// would teach the shape even though these particular strings are written
/// by tests. Prints a count instead, the same way `crate::build::BuildSpec`
/// prints `env`.
pub struct Sections {
    /// The sections to hand out, in order.
    sections: Vec<String>,
    /// How many have been asked for.
    asked: Cell<usize>,
}

impl core::fmt::Debug for Sections {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sections")
            .field(
                "sections",
                &format_args!("<{} sections>", self.sections.len()),
            )
            .field("asked", &self.asked.get())
            .finish()
    }
}

impl Sections {
    /// A shepherd handing out `first`, and then `rest`, in order.
    ///
    /// Split in two so that a shepherd with nothing to answer cannot be
    /// built. It took one slice and asserted the slice was not empty until
    /// review pointed out that naming a panic is not the same as not
    /// having one: with the first section required, the indexing in
    /// [`Self::dog_config`] has no empty case left to meet, so this
    /// constructor cannot fail at all rather than failing with a good
    /// message.
    ///
    /// [`Self::dog_config`]: crate::daemon::Daemon::dog_config
    pub fn of(first: &str, rest: &[&str]) -> Self {
        Self {
            sections: core::iter::once(first)
                .chain(rest.iter().copied())
                .map(str::to_owned)
                .collect(),
            asked: Cell::new(0),
        }
    }

    /// How many times the section has been asked for.
    pub fn asked(&self) -> usize {
        self.asked.get()
    }
}

impl Daemon for Sections {
    async fn dog_config(&self, _name: &str) -> Result<String, Error> {
        let asked = self.asked.get();
        self.asked.set(asked + 1);
        // Under a paused clock a loop that stopped sleeping never parks, so
        // the runtime never advances time and a timeout around it hangs
        // rather than firing. This turns that into a red test with a reason
        // on it, exactly as `poll`'s `Counting` double does.
        assert!(
            asked < 200,
            "the section was asked for far more often than the interval allows: the loop is \
             not sleeping"
        );
        Ok(self.sections[asked.min(self.sections.len() - 1)].clone())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        list_flock, describe, start, delete, reload, restart, save_roll, sheep_config,
        set_sheep_dog_settings, set_smit,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a `Sections` printed with `{:?}` shows a section body. A
    /// real `[dog.<name>]` section routinely carries webhook credentials
    /// for other dogs, so the hand-written `Debug` prints a count; this
    /// pins that exact shape, because a later `#[derive(Debug)]` here would
    /// be silent and would put whatever a test wrote into the failure
    /// output of every test that ever prints one.
    #[test]
    fn debug_does_not_print_the_sections() {
        let daemon = Sections::of("retention = 9", &["interval = \"hunter2-distinctive\""]);

        let shown = format!("{daemon:?}");

        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("retention"), "{shown}");
        assert_eq!(shown, "Sections { sections: <2 sections>, asked: 0 }");
    }
}
