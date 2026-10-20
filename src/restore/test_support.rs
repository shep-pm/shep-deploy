//! Doubles and fixtures the restore tests share.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};

use shep_client::RequestError;
use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
use shep_client::shep_core::status::ProcStatus;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::fixtures;
use crate::paths::Tree;
use crate::state::State;

/// Writes `state` as `sheep`'s `deploy.toml` under `home`.
pub(super) fn write_record(home: &Path, sheep: &str, state: &State) {
    let tree = Tree::for_sheep(home, sheep);
    fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
        .expect("create target dir");
    state.write(&tree.state_file()).expect("write state");
}

/// Writes a `deploy.toml` for `sheep` recording `origin_cwd` and
/// `origin_script`, as opt-in would have.
pub(super) fn write_target_with_origin(
    home: &Path,
    sheep: &str,
    origin_cwd: &str,
    origin_script: &str,
) {
    let state = State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        origin_cwd: Some(PathBuf::from(origin_cwd)),
        origin_script: Some(origin_script.to_owned()),
        checkout: PathBuf::from(origin_cwd),
        ..fixtures::state()
    };
    write_record(home, sheep, &state);
}

/// Writes a `deploy.toml` for `sheep` carrying the whole pre-adoption
/// definition, as `prepare` writes since 2026-09-04.
pub(super) fn write_target_with_full_origin(home: &Path, sheep: &str, origin: &AppConfig) {
    let cwd = origin.cwd.clone().expect("an origin with a cwd");
    let state = State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        origin_cwd: Some(PathBuf::from(&cwd)),
        origin_script: Some(origin.script.clone()),
        checkout: PathBuf::from(&cwd),
        origin: Some(origin.clone()),
        ..fixtures::state()
    };
    write_record(home, sheep, &state);
}

/// Writes a `deploy.toml` for `sheep` with no `origin_cwd` or
/// `origin_script`, as a dog-bootstrapped sheep has.
pub(super) fn write_target_with_origin_absent(home: &Path, sheep: &str) {
    let state = State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        checkout: PathBuf::from("/srv/deploy-tree"),
        ..fixtures::state()
    };
    write_record(home, sheep, &state);
}

/// How many of a `Recording`'s `start` calls get refused.
pub(super) enum Refuse {
    Never,
    FirstOnly,
    Always,
}

/// A [`Daemon`] double naming a fixed set of already-registered sheep.
///
/// `save_roll` answers with a roll naming every sheep it was
/// constructed with, `cwd` under the deploy tree and `script` an
/// arbitrary placeholder - what the shepherd is presumed to have had
/// registered before this dog's removal began - unless it was built
/// [`Self::with_unreadable_roll`], which writes a roll this crate's
/// `shep-core` refuses, so `roll::registered` fails with its own real,
/// crafted cause rather than this double inventing one.
///
/// `describe` answers [`Self::instances`] running instances for
/// whichever name is asked, refusing outright if
/// [`Self::describe_fails`]. `delete` succeeds until the
/// [`Self::delete_fails_at`]th call, then refuses every one after -
/// `Some(1)` refuses the very first delete; a higher number lets some
/// instances go before one fails partway through.
///
/// `calls()` records only `delete` and `start` calls that actually
/// landed, in order: those are the two that change the flock, and the
/// ordering this pins is between them. `save_roll` and `describe` are
/// reads and are not recorded, so a reordering of those does not break
/// the assertion.
pub(super) struct Recording {
    sheep: Vec<&'static str>,
    refuse: Refuse,
    instances: u32,
    describe_fails: bool,
    delete_fails_at: Option<u32>,
    unreadable_roll: bool,
    /// What the roll says each sheep's `cwd` is, when the default fixed
    /// path will not do. A cutover that registered points the sheep at its
    /// tree's own `current`, which only the test knows the path of.
    registered_cwd: Option<String>,
    calls: RefCell<Vec<&'static str>>,
    starts: RefCell<Vec<AppConfig>>,
    attempts: Cell<usize>,
    delete_attempts: Cell<u32>,
    /// Where `save_roll` writes, removed with the double.
    roll_dir: tempfile::TempDir,
}

impl Recording {
    pub(super) fn new(sheep: &[&'static str], refuse: Refuse) -> Self {
        Self {
            sheep: sheep.to_vec(),
            refuse,
            instances: 1,
            describe_fails: false,
            delete_fails_at: None,
            unreadable_roll: false,
            registered_cwd: None,
            calls: RefCell::new(Vec::new()),
            starts: RefCell::new(Vec::new()),
            attempts: Cell::new(0),
            delete_attempts: Cell::new(0),
            roll_dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    /// Every registered sheep accepts every start.
    pub(super) fn with_registered(sheep: &[&'static str]) -> Self {
        Self::new(sheep, Refuse::Never)
    }

    /// The same, with every sheep registered against `cwd` rather than
    /// the fixed path the roll otherwise reports.
    pub(super) fn registered_at(mut self, cwd: &std::path::Path) -> Self {
        self.registered_cwd = Some(cwd.display().to_string());
        self
    }

    /// The very first `start` call, across every sheep, is refused;
    /// every later one is accepted.
    pub(super) fn refusing_first_start_only(sheep: &[&'static str]) -> Self {
        Self::new(sheep, Refuse::FirstOnly)
    }

    /// Every `start` call is refused, for every sheep.
    pub(super) fn refusing_every_start(sheep: &[&'static str]) -> Self {
        Self::new(sheep, Refuse::Always)
    }

    /// `save_roll` answers with a roll `roll::registered` cannot parse,
    /// naming no particular sheep - a roll failure is dog-wide, so
    /// nothing here is keyed to the sheep the test writes to disk.
    pub(super) fn with_unreadable_roll() -> Self {
        let mut this = Self::new(&[], Refuse::Never);
        this.unreadable_roll = true;
        this
    }

    /// `describe` refuses outright, for every sheep. `put_back` cannot
    /// learn what to delete without it, so nothing is ever deleted or
    /// started.
    pub(super) fn refusing_describe(sheep: &[&'static str]) -> Self {
        let mut this = Self::new(sheep, Refuse::Never);
        this.describe_fails = true;
        this
    }

    /// A single instance is described, and its only `delete` refuses.
    /// Nothing lands before the failure, so the sheep is fully running
    /// and fully registered - the ordinary shape a refused delete
    /// takes, and the one that must stay `Failed` rather than
    /// `PartlyDeleted`, which needs a delete to have already landed.
    pub(super) fn refusing_first_delete_only(sheep: &[&'static str]) -> Self {
        let mut this = Self::new(sheep, Refuse::Never);
        this.delete_fails_at = Some(1);
        this
    }

    /// Two instances are described; the first `delete` lands, the
    /// second refuses. This is the only shape any test here gives more
    /// than one instance, deliberately: it is what exercises the branch
    /// where a delete fails PARTWAY through, with at least one instance
    /// already gone and another one still to account for.
    pub(super) fn refusing_delete_partway(sheep: &[&'static str]) -> Self {
        let mut this = Self::new(sheep, Refuse::Never);
        this.instances = 2;
        this.delete_fails_at = Some(2);
        this
    }

    pub(super) fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }

    pub(super) fn started(&self) -> Vec<AppConfig> {
        self.starts.borrow().clone()
    }
}

impl Daemon for Recording {
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        if self.describe_fails {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "describe refused".to_owned(),
                daemon_version: None,
            })));
        }
        Ok((0..self.instances)
            .map(|offset| {
                ProcessInfo::builder(offset + 1, sheep, ProcStatus::Online)
                    .pid(Some(1000 + offset))
                    .build()
            })
            .collect())
    }
    async fn start(&self, apps: Vec<AppConfig>) -> Result<Vec<u32>, Error> {
        let attempt = self.attempts.get();
        self.attempts.set(attempt + 1);
        let refused = match self.refuse {
            Refuse::Never => false,
            Refuse::FirstOnly => attempt == 0,
            Refuse::Always => true,
        };
        // Recorded whether accepted or refused: `started()` is what a
        // test asserts the config a call was ATTEMPTED with, including
        // the refused restore itself.
        self.starts.borrow_mut().extend(apps);
        if refused {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "refused".to_owned(),
                daemon_version: None,
            })));
        }
        // `calls()` tracks only what actually changed the flock, so a
        // refused start does not appear here.
        self.calls.borrow_mut().push("start");
        Ok(Vec::new())
    }
    async fn delete(&self, _id: u32) -> Result<(), Error> {
        let attempt = self.delete_attempts.get() + 1;
        self.delete_attempts.set(attempt);
        if self.delete_fails_at == Some(attempt) {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "delete refused".to_owned(),
                daemon_version: None,
            })));
        }
        self.calls.borrow_mut().push("delete");
        Ok(())
    }
    async fn save_roll(&self) -> Result<PathBuf, Error> {
        let path = self.roll_dir.path().join("flock.json");
        if self.unreadable_roll {
            // A field of the wrong type, which is what `AppConfig`
            // still refuses. An unknown field was the trigger until
            // shep-core 0.7 dropped `deny_unknown_fields`, at which
            // point this roll became readable and the test below
            // asserted nothing. Either way the point is the same: the
            // roll fails on its own crafted, actionable cause rather
            // than on one this double invented.
            fs::write(&path, "{\"apps\":[{\"app\":{\"name\":123}}]}").expect("write roll");
            return Ok(path);
        }
        let apps: Vec<String> = self
            .sheep
            .iter()
            .map(|name| {
                let cwd = self
                    .registered_cwd
                    .clone()
                    .unwrap_or_else(|| "/srv/deploy-tree/current".to_owned());
                format!(
                    "{{\"app\":{{\"name\":{name:?},\"script\":\"the-shepherds-own-script\",\
                     \"cwd\":{cwd:?}}}}}"
                )
            })
            .collect();
        fs::write(&path, format!("{{\"apps\":[{}]}}", apps.join(","))).expect("write roll");
        Ok(path)
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, reload, restart, set_smit,
    );
}

/// Writes a `deploy.toml` as `optin::prepare` leaves one when its cutover
/// never ran: an origin recorded, and no `deployed`.
pub(super) fn write_target_never_cut_over(
    home: &Path,
    sheep: &str,
    origin_cwd: &str,
    script: &str,
) {
    let state = State {
        origin_cwd: Some(PathBuf::from(origin_cwd)),
        origin_script: Some(script.to_owned()),
        checkout: PathBuf::from(origin_cwd),
        ..fixtures::state()
    };
    write_record(home, sheep, &state);
}
