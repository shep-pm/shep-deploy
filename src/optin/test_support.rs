//! Doubles and fixtures the opt-in tests share.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_client::RequestError;
use shep_client::shep_core::config::{AppConfig, DogTable};
use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
use shep_client::shep_core::status::ProcStatus;
use tokio::time::Instant;

use super::prepare::{Prepared, prepare};
use crate::daemon::Daemon;
use crate::error::Error;
use crate::fixtures;
use crate::paths::Tree;
use crate::settings::Settings;
use crate::state::{State, Watch};

/// A [`Daemon`] whose `save_roll` writes a roll JSON naming each
/// `(sheep, cwd)` pair, with `script = "./run.sh"`, into a fresh
/// tempdir, and answers with the path it wrote. `sheep_config` and
/// `set_sheep_dog_settings` act on the first sheep's config, so a test reads
/// back what `prepare` wrote to its table. Every other method is
/// `unimplemented!()`, since `prepare` never calls them.
pub(super) struct RollOf<'a> {
    entries: &'a [(&'a str, &'a Path)],
    held: fixtures::Sheep,
}

impl<'a> RollOf<'a> {
    pub(super) fn new(entries: &'a [(&'a str, &'a Path)]) -> Self {
        let (name, _) = entries.first().expect("a roll names at least one sheep");
        Self {
            entries,
            held: fixtures::Sheep::holding(fixtures::app_named(name)),
        }
    }

    /// The first sheep as the shepherd holds it after `prepare`.
    pub(super) fn app(&self) -> AppConfig {
        self.held.app()
    }

    /// The first sheep's `[app.dogs.deploy]` table as the shepherd holds it.
    pub(super) fn settings(&self) -> Settings {
        self.held.settings()
    }
}

impl Daemon for RollOf<'_> {
    async fn save_roll(&self) -> Result<PathBuf, Error> {
        write_roll(self.entries, "")
    }

    async fn sheep_config(&self, _sheep: &str) -> Result<AppConfig, Error> {
        self.held.answer()
    }

    async fn set_sheep_dog_settings(
        &self,
        _sheep: &str,
        dog: &str,
        table: DogTable,
    ) -> Result<(), Error> {
        self.held.replace(dog, table)
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, describe, start, delete, reload, restart, set_smit,
    );
}

/// [`RollOf`], for sheep registered with an `[app.dogs.deploy]` whose build
/// leaves `marker` behind if it ever runs.
pub(super) struct RollBuilding<'a> {
    entries: &'a [(&'a str, &'a Path)],
    marker: &'a Path,
    held: fixtures::Sheep,
}

impl<'a> RollBuilding<'a> {
    pub(super) fn new(entries: &'a [(&'a str, &'a Path)], marker: &'a Path) -> Self {
        let (name, _) = entries.first().expect("a roll names at least one sheep");
        let command = format!("touch {}", marker.display());
        Self {
            entries,
            marker,
            held: fixtures::Sheep::of(name, &format!("command = {command:?}\n")),
        }
    }

    /// The first sheep's `[app.dogs.deploy]` table as the shepherd holds it.
    pub(super) fn settings(&self) -> Settings {
        self.held.settings()
    }
}

impl Daemon for RollBuilding<'_> {
    async fn save_roll(&self) -> Result<PathBuf, Error> {
        let command = format!("touch {}", self.marker.display());
        write_roll(
            self.entries,
            &format!(",\"dogs\":{{\"deploy\":{{\"command\":{command:?}}}}}"),
        )
    }

    async fn sheep_config(&self, _sheep: &str) -> Result<AppConfig, Error> {
        self.held.answer()
    }

    async fn set_sheep_dog_settings(
        &self,
        _sheep: &str,
        dog: &str,
        table: DogTable,
    ) -> Result<(), Error> {
        self.held.replace(dog, table)
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, describe, start, delete, reload, restart, set_smit,
    );
}

/// Writes a roll JSON naming each `(sheep, cwd)` pair, with `extra` spliced
/// into every app after its `cwd`, and answers with the path.
fn write_roll(entries: &[(&str, &Path)], extra: &str) -> Result<PathBuf, Error> {
    let apps: Vec<String> = entries
        .iter()
        .map(|(name, cwd)| {
            format!(
                "{{\"app\":{{\"name\":{name:?},\"script\":\"./run.sh\",\"cwd\":{:?}{extra}}}}}",
                cwd.to_str().expect("utf-8 cwd")
            )
        })
        .collect();
    // Written inside the first checkout rather than into a tempdir
    // that has to be kept alive past this call and was therefore
    // never removed, one per `prepare`. The checkout is a test's own
    // tempdir and goes with it; an untracked file in it is not
    // ignored, so nothing links it into a release.
    let (_, cwd) = entries.first().expect("a roll names at least one sheep");
    let path = cwd.join(".flock-roll.json");
    std::fs::write(&path, format!("{{\"apps\":[{}]}}", apps.join(","))).expect("write roll");
    Ok(path)
}

/// A tempdir that is a git checkout, on branch `main`, with an `origin`
/// remote pointing at itself and one commit declaring an app named
/// `bpm` whose script is `./run.sh`.
///
/// `origin` points at the checkout's own path rather than a second
/// fixture repository: `prepare` fetches from whatever `git::remote_url`
/// reads, and a checkout is a perfectly fetchable remote for its own
/// history.
pub(super) fn checkout_with_commit() -> tempfile::TempDir {
    checkout_declaring("[[app]]\nname = \"bpm\"\nscript = \"./run.sh\"\nwait_ready = true\n")
}

/// As [`checkout_with_commit`], with the Flockfile's text given.
pub(super) fn checkout_declaring(flockfile: &str) -> tempfile::TempDir {
    let dir = fixtures::checkout(&[
        ("Flockfile.toml", flockfile),
        ("run.sh", "#!/bin/sh\necho hi\n"),
    ]);
    // Itself, so `git::remote_url` has an answer. Nothing fetches from it.
    fixtures::run_git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            dir.path().to_str().expect("utf-8 tempdir path"),
        ],
    );
    dir
}

/// Writes `<home>/deploy/<sheep>/deploy.toml` through `State::write`,
/// standing in for a sheep that is already a deploy target.
pub(super) fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
    let tree = Tree::for_sheep(home, sheep);
    std::fs::create_dir_all(tree.state_file().parent().expect("has a parent"))
        .expect("create target dir");
    let state = State {
        deployed: sha.map(str::to_owned),
        watch,
        ..fixtures::state()
    };
    state.write(&tree.state_file()).expect("write state");
}

// ---- the cutover ----------------------------------------------------

/// The pid of the first pre-existing instance; a second counts up from
/// it.
pub(super) const ORIGINAL_PID: u32 = 100;
/// The id of the first instance the cutover's own `Start` spawns.
///
/// A second newcomer counts DOWN from it, so no id in these fixtures is
/// ever also a pid. An assertion aimed at the wrong one then fails
/// rather than coinciding.
pub(super) const NEWCOMER_ID: u32 = 99;
/// The pid of the first newcomer; a second counts up from it.
pub(super) const NEWCOMER_PID: u32 = 200;
/// The id of the instance a repair `Start` necessarily spawns.
pub(super) const REPAIR_ID: u32 = 500;
/// The pid of that instance.
pub(super) const REPAIR_PID: u32 = 700;

/// What one row looks like at some moment after the `Start`.
///
/// Written for the newcomers and used for the originals too, because
/// the case that matters most is an original that stops looking like
/// itself: see `an_original_that_respawns_is_not_mistaken_for_the_newcomer`.
#[derive(Debug, Clone, Copy)]
pub(super) enum Shape {
    /// No row at all. This is what the first poll or two really see: a
    /// `Start` is an acceptance, and the shepherd spawns afterwards.
    Absent,
    /// A row, at this status, pid and restart count.
    Up {
        status: ProcStatus,
        pid: u32,
        restarts: u32,
    },
}

/// A row's shape at `elapsed`, from a step function written as
/// `(from, shape)` pairs in ascending order.
pub(super) fn shape_at(script: &[(Duration, Shape)], elapsed: Duration) -> Shape {
    script
        .iter()
        .rev()
        .find(|(from, _)| *from <= elapsed)
        .map_or(Shape::Absent, |(_, shape)| *shape)
}

/// The ordinary original: `Online` at its own pid, never restarted,
/// for as long as the cutover runs.
pub(super) fn originals_stay_online() -> Vec<(Duration, Shape)> {
    vec![(
        Duration::ZERO,
        Shape::Up {
            status: ProcStatus::Online,
            pid: ORIGINAL_PID,
            restarts: 0,
        },
    )]
}

/// An original that crashes and is respawned a second into the cutover:
/// same instance id, a new pid, `restarts` at one.
///
/// The shape a port fight really produces on the losing side, and the
/// one that reads as a newcomer to a pid-only comparison.
pub(super) fn originals_respawn_mid_cutover() -> Vec<(Duration, Shape)> {
    vec![
        (
            Duration::ZERO,
            Shape::Up {
                status: ProcStatus::Online,
                pid: ORIGINAL_PID,
                restarts: 0,
            },
        ),
        (
            Duration::from_secs(1),
            Shape::Up {
                status: ProcStatus::Online,
                pid: ORIGINAL_PID + 50,
                restarts: 1,
            },
        ),
    ]
}

/// A [`Daemon`] that answers `describe` the way a real shepherd does
/// through a cutover, and records every `start` and `delete`.
///
/// The originals stay `Online` unless a fixture says otherwise, which is
/// not a convenience: `Request::Start` on a registered name ADDS an
/// instance beside them, and a real shepherd keeps the old one serving
/// until something removes it. A double that turned the flock over
/// instantly would be the same fiction that let the engine plan's worst
/// blocker survive unit testing.
///
/// Both scripts are a function of elapsed time since the `Start`, so a
/// fixture says what the flock DOES rather than counting polls: under
/// `start_paused` the clock moves only when the code under test sleeps,
/// so `attempt`'s 100ms poll and its ten-second dwell land on the
/// script's own thresholds deterministically.
pub(super) struct CutOverDouble {
    /// The ids the shepherd already has for this sheep.
    originals: Vec<u32>,
    /// What every original looks like as time passes since the `Start`,
    /// and before it.
    original_script: Vec<(Duration, Shape)>,
    /// What each newcomer looks like as time passes since the `Start`.
    script: Vec<(Duration, Shape)>,
    /// Which `start` call, counting from zero, the shepherd refuses.
    refuses: Option<usize>,
    /// Which `delete` call, counting from zero, the shepherd starts
    /// refusing at. Every one from there on is refused.
    refuses_deletes_from: Option<usize>,
    /// How many `delete` calls have arrived, refused ones included.
    deletes_seen: Cell<usize>,
    /// How long after the `Start` every `describe` starts failing.
    mute_after: Option<Duration>,
    /// The sheep as the shepherd holds it, which `sheep_config` answers.
    registered: fixtures::Sheep,
    /// Every `start` the shepherd accepted, in order.
    starts: RefCell<Vec<AppConfig>>,
    /// Every id passed to `delete`, in order.
    deletes: RefCell<Vec<u32>>,
    /// How many `start` calls have arrived, refused ones included.
    attempts: Cell<usize>,
    /// When the cutover's own `Start` was accepted.
    accepted_at: Cell<Option<Instant>>,
    /// At most this many newcomers appear, whatever the `Start` asked
    /// for: a shepherd that spawned one of two and no more.
    newcomers_at_most: Option<u32>,
    /// How many instances the repair `Start`s have spawned.
    repairs: Cell<u32>,
    /// When the first repair `Start` was accepted; its instance appears
    /// [`SPAWN_LAG`] after, as a real shepherd's would.
    repaired_at: Cell<Option<Instant>>,
    /// How far apart a scaled repair's rows appear, if not together.
    repair_stagger: Option<Duration>,
    /// When a newcomer `newcomers_at_most` held back finally appears.
    late_newcomer_after: Option<Duration>,
}

/// How long a shepherd takes to spawn after accepting a `Start`, as the
/// double plays it.
pub(super) const SPAWN_LAG: Duration = Duration::from_millis(250);

impl CutOverDouble {
    pub(super) fn new(
        originals: &[u32],
        script: Vec<(Duration, Shape)>,
        refuses: Option<usize>,
    ) -> Self {
        Self {
            originals: originals.to_vec(),
            original_script: originals_stay_online(),
            script,
            refuses,
            refuses_deletes_from: None,
            deletes_seen: Cell::new(0),
            mute_after: None,
            registered: fixtures::Sheep::holding(fixtures::app_named("bpm")),
            starts: RefCell::new(Vec::new()),
            deletes: RefCell::new(Vec::new()),
            attempts: Cell::new(0),
            accepted_at: Cell::new(None),
            newcomers_at_most: None,
            repairs: Cell::new(0),
            repaired_at: Cell::new(None),
            repair_stagger: None,
            late_newcomer_after: None,
        }
    }

    /// The same double, holding the sheep as `registered`.
    pub(super) fn registered_as(mut self, registered: AppConfig) -> Self {
        self.registered = fixtures::Sheep::holding(registered);
        self
    }

    /// The same double, with one newcomer the cap held back appearing
    /// `after` the cutover's `Start` was accepted.
    pub(super) fn with_a_late_newcomer_after(mut self, after: Duration) -> Self {
        self.late_newcomer_after = Some(after);
        self
    }

    /// The same double, with a scaled repair's rows appearing one per
    /// `gap` rather than all on the poll after the spawn lag.
    pub(super) fn staggering_repairs_by(mut self, gap: Duration) -> Self {
        self.repair_stagger = Some(gap);
        self
    }

    /// The same double, spawning at most `n` newcomers however many the
    /// `Start` asked for.
    pub(super) fn spawning_at_most(mut self, n: u32) -> Self {
        self.newcomers_at_most = Some(n);
        self
    }

    /// The same double, with every `describe` failing from `after`
    /// onwards - a shepherd that stops answering mid-cutover.
    pub(super) fn going_quiet_after(mut self, after: Duration) -> Self {
        self.mute_after = Some(after);
        self
    }

    /// The same double, with every `delete` refused.
    pub(super) fn refusing_deletes(self) -> Self {
        self.refusing_deletes_from(0)
    }

    /// The same double, refusing every `delete` from the `from`th on.
    ///
    /// `undo_start` drains twice - once for what the cutover's own
    /// `Start` spawned, once for what the repair `Start` did - and only
    /// the second failing is a shape no other fixture reaches.
    pub(super) fn refusing_deletes_from(mut self, from: usize) -> Self {
        self.refuses_deletes_from = Some(from);
        self
    }

    /// The same double, with the originals doing something other than
    /// sitting still.
    pub(super) fn while_the_originals(mut self, script: Vec<(Duration, Shape)>) -> Self {
        self.original_script = script;
        self
    }

    /// The same double, after an operator set `key` in the sheep's table.
    pub(super) fn with_operator_setting(self, key: &str, value: &str) -> Self {
        self.registered.set(key, value);
        self
    }

    /// The sheep's `[app.dogs.deploy]` table as the shepherd holds it now.
    pub(super) fn settings(&self) -> Settings {
        self.registered.settings()
    }

    /// Every app the shepherd accepted a `Start` for, in order.
    pub(super) fn started(&self) -> Vec<AppConfig> {
        self.starts.borrow().clone()
    }

    /// Every id the cutover deleted, in order.
    pub(super) fn deleted(&self) -> Vec<u32> {
        self.deletes.borrow().clone()
    }

    pub(super) fn is_deleted(&self, id: u32) -> bool {
        self.deletes.borrow().contains(&id)
    }

    fn row(&self, id: u32, status: ProcStatus, pid: u32, restarts: u32) -> ProcessInfo {
        let _ = self;
        ProcessInfo::builder(id, "bpm", status)
            .pid(Some(pid))
            .restarts(restarts)
            .build()
    }
}

impl Daemon for CutOverDouble {
    async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        let mut flock = Vec::new();
        // Before the `Start` the originals are read at zero, which is
        // where every script has them sitting still.
        let elapsed = self
            .accepted_at
            .get()
            .map_or(Duration::ZERO, |accepted_at| Instant::now() - accepted_at);

        if self.accepted_at.get().is_some() && self.mute_after.is_some_and(|after| elapsed >= after)
        {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "the shepherd is not answering".to_owned(),
                daemon_version: None,
            })));
        }

        if let Shape::Up {
            status,
            pid,
            restarts,
        } = shape_at(&self.original_script, elapsed)
        {
            for (offset, id) in self.originals.iter().enumerate() {
                let offset = u32::try_from(offset).expect("a handful of instances");
                if !self.is_deleted(*id) {
                    flock.push(self.row(*id, status, pid + offset, restarts));
                }
            }
        }

        if self.accepted_at.get().is_some()
            && let Shape::Up {
                status,
                pid,
                restarts,
            } = shape_at(&self.script, elapsed)
        {
            // As many as the accepted `Start` asked for, which is what a
            // shepherd spawns; the originals' count is a different
            // number and using it hid the multi-instance wait entirely.
            let asked = self
                .starts
                .borrow()
                .first()
                .map_or(1, |app| app.instances.max(1));
            let spawned = self.newcomers_at_most.map_or(asked, |cap| asked.min(cap));
            for offset in 0..spawned {
                let id = NEWCOMER_ID - offset;
                if !self.is_deleted(id) {
                    flock.push(self.row(id, status, pid + offset, restarts));
                }
            }
            // A newcomer the cap held back, arriving late: a shepherd
            // that spawned the rest of what it registered only after the
            // cutover had given up on waiting.
            if let Some(late) = self.late_newcomer_after
                && elapsed >= late
                && spawned < asked
            {
                let id = NEWCOMER_ID - spawned;
                if !self.is_deleted(id) {
                    flock.push(self.row(id, ProcStatus::Online, pid + spawned, 0));
                }
            }
        }

        // Not at once. `undo_start` used to describe right after the
        // repair `Start` and find nothing, and a double that showed the
        // row instantly hid that.
        if let Some(at) = self.repaired_at.get() {
            let since = Instant::now() - at;
            for offset in 0..self.repairs.get() {
                // The offset-th row lands `offset` gaps after the first.
                let lands_at = SPAWN_LAG + self.repair_stagger.unwrap_or(Duration::ZERO) * offset;
                let id = REPAIR_ID + offset;
                if since >= lands_at && !self.is_deleted(id) {
                    flock.push(self.row(id, ProcStatus::Online, REPAIR_PID + offset, 0));
                }
            }
        }

        Ok(flock)
    }
    async fn sheep_config(&self, _sheep: &str) -> Result<AppConfig, Error> {
        self.registered.answer()
    }
    async fn set_sheep_dog_settings(
        &self,
        _sheep: &str,
        dog: &str,
        table: DogTable,
    ) -> Result<(), Error> {
        self.registered.replace(dog, table)
    }
    async fn start(&self, apps: Vec<AppConfig>) -> Result<Vec<u32>, Error> {
        let attempt = self.attempts.get();
        self.attempts.set(attempt + 1);
        if self.refuses == Some(attempt) {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "bpm cannot be started".to_owned(),
                daemon_version: None,
            })));
        }

        // One row per instance asked for, registered now and spawned
        // later, which is what a shepherd answers a `Start` with.
        let asked = apps.first().map_or(1, |app| app.instances.max(1));
        self.starts.borrow_mut().extend(apps);
        if attempt == 0 {
            self.accepted_at.set(Some(Instant::now()));
            Ok((0..asked).map(|offset| NEWCOMER_ID - offset).collect())
        } else {
            let first = self.repairs.get();
            self.repairs.set(first + asked);
            if self.repaired_at.get().is_none() {
                self.repaired_at.set(Some(Instant::now()));
            }
            Ok((first..first + asked)
                .map(|offset| REPAIR_ID + offset)
                .collect())
        }
    }
    async fn delete(&self, id: u32) -> Result<(), Error> {
        let seen = self.deletes_seen.get();
        self.deletes_seen.set(seen + 1);
        if self.refuses_deletes_from.is_some_and(|from| seen >= from) {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: format!("instance {id} cannot be deleted"),
                daemon_version: None,
            })));
        }
        self.deletes.borrow_mut().push(id);
        Ok(())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, reload, restart, save_roll, set_smit,
    );
}

/// The tempdirs a fixture has to keep alive. `Prepared` names paths
/// inside both, and a `TempDir` deletes its directory when it drops.
pub(super) struct Dirs {
    _home: tempfile::TempDir,
    _checkout: tempfile::TempDir,
}

/// A prepared tree for `bpm`, plus a double scripted with `script`.
pub(super) async fn cutover_fixture_of(
    originals: &[u32],
    script: Vec<(Duration, Shape)>,
    refuses: Option<usize>,
) -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_from(checkout_with_commit(), originals, script, refuses).await
}

/// As [`cutover_fixture_of`], from a checkout the test built itself.
pub(super) async fn cutover_fixture_from(
    checkout: tempfile::TempDir,
    originals: &[u32],
    script: Vec<(Duration, Shape)>,
    refuses: Option<usize>,
) -> (CutOverDouble, Prepared, Dirs) {
    let home = tempfile::tempdir().expect("tempdir");
    let entries = [("bpm", checkout.path())];
    let roll = RollOf::new(&entries);
    let prepared = prepare(&roll, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    (
        CutOverDouble::new(originals, script, refuses).registered_as(roll.app()),
        prepared,
        Dirs {
            _home: home,
            _checkout: checkout,
        },
    )
}

/// The ordinary shape: no row for the first few polls, then `Starting`,
/// then `Online`, and it stays that way through the dwell.
///
/// Staged rather than instant on purpose. `attempt` polls every 100ms,
/// so a newcomer that only appears at 250ms is adopted on the fourth
/// poll, which is what a shepherd that has accepted a `Start` and not
/// yet spawned looks like.
pub(super) fn comes_up() -> Vec<(Duration, Shape)> {
    vec![
        (Duration::ZERO, Shape::Absent),
        (
            Duration::from_millis(250),
            Shape::Up {
                status: ProcStatus::Starting,
                pid: NEWCOMER_PID,
                restarts: 0,
            },
        ),
        (
            Duration::from_millis(450),
            Shape::Up {
                status: ProcStatus::Online,
                pid: NEWCOMER_PID,
                restarts: 0,
            },
        ),
    ]
}

/// One original, and a newcomer that comes up and stays.
pub(super) async fn cutover_fixture() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(&[7], comes_up(), None).await
}

/// Several originals, each replaced by a newcomer that comes up and
/// stays.
pub(super) async fn cutover_fixture_with_instances(
    originals: &[u32],
) -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(originals, comes_up(), None).await
}

/// A newcomer that reports `Online` at once and is gone by the dwell.
///
/// The exact shape finding F1 describes: shep marks a FRESH spawn
/// `Online` when `listen_timeout` elapses whatever the readiness probe
/// said, so a release that is already dead reports `Online` on time.
pub(super) async fn cutover_fixture_online_then_gone() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(
        &[7],
        vec![
            (
                Duration::ZERO,
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (Duration::from_secs(5), Shape::Absent),
        ],
        None,
    )
    .await
}

/// A newcomer present at every poll, under a new pid by the dwell.
///
/// `Online` rather than `Starting`, because that is what a crash loop
/// really looks like: each spawn reaches `Online` on its
/// `listen_timeout` and then dies, so status alone never says anything
/// is wrong and only the pid does.
pub(super) async fn cutover_fixture_crash_looping() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(
        &[7],
        vec![
            (Duration::ZERO, Shape::Absent),
            (
                Duration::from_millis(250),
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID,
                    restarts: 0,
                },
            ),
            (
                Duration::from_secs(5),
                Shape::Up {
                    status: ProcStatus::Online,
                    pid: NEWCOMER_PID + 50,
                    restarts: 1,
                },
            ),
        ],
        None,
    )
    .await
}

/// A newcomer that comes up and is `Errored` by the dwell: the
/// port-collision shape, where the original still holds the port.
pub(super) fn dies_during_dwell() -> Vec<(Duration, Shape)> {
    vec![
        (Duration::ZERO, Shape::Absent),
        (
            Duration::from_millis(250),
            Shape::Up {
                status: ProcStatus::Online,
                pid: NEWCOMER_PID,
                restarts: 0,
            },
        ),
        (
            Duration::from_secs(5),
            Shape::Up {
                status: ProcStatus::Errored,
                pid: NEWCOMER_PID,
                restarts: 0,
            },
        ),
    ]
}

/// The port-collision shape, with the repair `Start` accepted.
pub(super) async fn cutover_fixture_dies_during_dwell() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(&[7], dies_during_dwell(), None).await
}

/// The same, with every `start` after the cutover's own refused.
///
/// The SECOND call, not the first: the cutover's own `Start` has to be
/// accepted or the run never reaches the repair this exists to check.
pub(super) async fn cutover_fixture_dies_and_refuses_repair() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(&[7], dies_during_dwell(), Some(1)).await
}

/// A newcomer that never appears at all, so the phase-one budget is
/// what ends the wait.
///
/// Absent rather than stuck in `Starting`: `is_alive` accepts
/// `Starting`, deliberately, because a probeless app sits there for its
/// whole `listen_timeout` and that is not a failure. What the budget
/// bounds is a newcomer APPEARING.
pub(super) async fn cutover_fixture_never_appears() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(&[7], vec![(Duration::ZERO, Shape::Absent)], None).await
}

/// A newcomer that comes up and stays, with every `delete` refused.
pub(super) async fn cutover_fixture_comes_up_but_refuses_deletes() -> (CutOverDouble, Prepared, Dirs)
{
    let (daemon, prepared, dirs) = cutover_fixture_of(&[7, 8], comes_up(), None).await;
    (daemon.refusing_deletes(), prepared, dirs)
}

/// A shepherd that accepts the `Start` and then stops answering.
///
/// Muted from the instant the `Start` lands, which is the worst
/// version: the roll has been poisoned and every request that could
/// read or repair it fails.
pub(super) async fn cutover_fixture_shepherd_goes_quiet() -> (CutOverDouble, Prepared, Dirs) {
    let (daemon, prepared, dirs) = cutover_fixture_of(&[7], comes_up(), None).await;
    (daemon.going_quiet_after(Duration::ZERO), prepared, dirs)
}

/// The port-collision shape, with only the SECOND `delete` refused:
/// the one that removes the instance the repair `Start` spawned.
///
/// The roll is recorded, so `repaired` is true, and an instance is
/// still left registered, so `removed` is false. No other fixture
/// reaches that combination, and it is the one where a cwd-based hint
/// would point at the wrong instance: the repair `Start` carries the
/// ORIGINAL config, so what it spawned has the operator's own checkout
/// as its cwd, exactly like the survivor.
pub(super) async fn cutover_fixture_dies_and_refuses_the_second_delete()
-> (CutOverDouble, Prepared, Dirs) {
    let (daemon, prepared, dirs) = cutover_fixture_of(&[7], dies_during_dwell(), None).await;
    (daemon.refusing_deletes_from(1), prepared, dirs)
}

/// The port-collision shape, with every `delete` refused as well.
pub(super) async fn cutover_fixture_dies_and_refuses_deletes() -> (CutOverDouble, Prepared, Dirs) {
    let (daemon, prepared, dirs) = cutover_fixture_of(&[7], dies_during_dwell(), None).await;
    (daemon.refusing_deletes(), prepared, dirs)
}

/// An original that crashes and respawns a second into the cutover,
/// with no real newcomer ever arriving.
///
/// The two halves are both needed. The respawn gives the original a pid
/// the pre-`Start` generation never had, which is all `is_new` looks
/// at; the absent newcomer is what leaves it as the only candidate.
pub(super) async fn cutover_fixture_original_respawns() -> (CutOverDouble, Prepared, Dirs) {
    let (daemon, prepared, dirs) =
        cutover_fixture_of(&[7], vec![(Duration::ZERO, Shape::Absent)], None).await;
    (
        daemon.while_the_originals(originals_respawn_mid_cutover()),
        prepared,
        dirs,
    )
}

/// A shepherd that refuses the cutover's `Start` outright.
pub(super) async fn cutover_fixture_refusing_start() -> (CutOverDouble, Prepared, Dirs) {
    cutover_fixture_of(&[7], comes_up(), Some(0)).await
}
