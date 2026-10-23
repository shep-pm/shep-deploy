//! What every `deploy` submodule's tests share: a deploy target on disk and a
//! fake shepherd to deploy against.

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use shep_client::RequestError;
use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
use shep_client::shep_core::status::ProcStatus;
use tempfile::TempDir;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::fixtures;
use crate::paths::Tree;
use crate::state::{State, Watch};
use crate::swap;

/// [`fixtures::dog_config`] with a retention the caller cares about.
///
/// Separate because the retention tests are the only ones for which the
/// number means anything, and burying it in the shared default would
/// make those tests read as if any config would do.
pub(super) fn test_config_keeping(retention: usize) -> crate::config::DogConfig {
    crate::config::DogConfig {
        retention,
        ..fixtures::dog_config()
    }
}

/// The fixture app: one sheep named `web`, with a readiness probe,
/// because `Verify::Probed` refuses a sheep without one and `Probed` is
/// the default every test here runs under.
///
/// The probe's own target is never executed by anything in this file -
/// no test here runs a real shepherd - so `true` is honest rather than
/// lazy: what these tests need is an app that HAS a probe.
pub(crate) const FLOCKFILE: &str = "[[app]]\nname = 'web'\nscript = './run.sh'\n\n\
                         [app.readiness_probe]\nkind = 'http'\ntarget = 'http://127.0.0.1:1/health'\n";

/// A deploy target with a bare clone, an origin repo, and one release
/// already live.
pub(crate) struct Fixture {
    /// The `$SHEP_HOME` the tree is rooted in. Held to keep it alive
    /// for the test's duration, and read by the one test that needs a
    /// second spelling of the same directory.
    pub(crate) home: TempDir,
    pub(crate) origin: TempDir,
    pub(crate) tree: Tree,
    pub(crate) state: State,
}

/// A sha whose release directory is not on disk.
///
/// What a record naming a release retention has already reclaimed looks
/// like, and the only way a real target reaches "there is nothing to
/// roll back to" now that `deploy` refuses a tree the cutover never
/// landed on. A `deployed` of `None` used to stand in for this, and no
/// longer can.
pub(super) const RECLAIMED: &str = "0000000000000000000000000000000000000000";

/// An origin repo with a Flockfile declaring one app named `web`, and
/// a bare clone fetched from it - but no release built and no
/// `current` at all, which is a target the moment before its first
/// deploy.
pub(crate) fn fixture_before_any_release() -> Fixture {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = fixtures::checkout(&[("Flockfile.toml", FLOCKFILE)]);

    let tree = Tree::for_sheep(home.path(), "web");
    fs::create_dir_all(tree.git()).expect("create git dir");
    fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);

    let remote = origin.path().to_str().expect("utf-8 path").to_owned();
    crate::git::fetch(&tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");

    let state = State {
        remote,
        verify: crate::state::Verify::Probed,
        // What `crate::optin::prepare` writes: a tree nothing has
        // been served from yet is not the poll loop's.
        watch: crate::state::Watch::Manual,
        checkout: origin.path().to_owned(),
        ..fixtures::state()
    };

    // A real target always has one on disk, because that is where its
    // `State` was read from. Writing it here keeps assertions about
    // what `deploy.toml` says meaningful rather than vacuous.
    state.write(&tree.state_file()).expect("write deploy.toml");

    Fixture {
        home,
        origin,
        tree,
        state,
    }
}

/// [`fixture_before_any_release`] with its first commit already built,
/// live under `current`, and recorded in the state.
pub(crate) fn fixture_with_previous_release() -> Fixture {
    let mut fixture = fixture_before_any_release();
    let first = crate::git::remote_head(&fixture.tree.git(), "main").expect("head");
    install_release(&fixture, &first);
    fixture.state.deployed = Some(first);
    fixture
        .state
        .write(&fixture.tree.state_file())
        .expect("write deploy.toml");
    fixture
}

/// Builds `sha`'s worktree and points `current` at it, the way a
/// successful deploy would have.
pub(super) fn install_release(fixture: &Fixture, sha: &str) {
    let tree = &fixture.tree;
    if !tree.release(sha).exists() {
        crate::git::worktree_add(&tree.git(), &tree.release(sha), sha).expect("worktree");
    }
    swap::point_at(&tree.current(), &tree.release(sha)).expect("swap");
}

/// Adds a commit to the fixture's origin and returns its sha.
pub(crate) fn commit_on_origin(fixture: &Fixture, name: &str) -> String {
    fs::write(fixture.origin.path().join(name), "x").expect("write");
    fixtures::run_git(fixture.origin.path(), &["add", "."]);
    fixtures::run_git(fixture.origin.path(), &["commit", "-q", "-m", name]);
    fixtures::head_of(fixture.origin.path())
}

/// A [`Daemon`] that models what a reload actually does to the flock:
/// it replaces the running process with a different one, under a
/// different pid. Counts its reloads, because a rollback that moves the
/// symlink and never reloads is invisible to every filesystem
/// assertion.
///
/// A fake that answered `describe` however a test wanted is how the
/// generation bug survived two review rounds: verification asked "is
/// anything Online" and a fake always had something Online to show it.
/// Every constructor below therefore describes a real shepherd
/// behaviour rather than a convenient answer, and
/// [`Self::keeps_the_old_instance`] is the one checked against a real
/// shepherd.
pub(crate) struct Shepherd {
    /// What the flock settles to after a reload: `Online` is a release
    /// that came up, `Starting` one that spawned and never became
    /// ready.
    settles_to: ProcStatus,
    /// Whether a reload actually puts a new process in place. `false`
    /// is shep's own behaviour when `AwaitReady` fails: it keeps the
    /// old instance serving, so the listing settles back to one
    /// `Online` entry under the pid that was already there.
    replaces: bool,
    /// What the flock settles to after the SECOND reload, which is a
    /// rollback's. `Online` by default, so a rollback comes up and the
    /// tests of everything else keep their meaning.
    rollback_settles_to: ProcStatus,
    /// As [`Self::replaces`], for the rollback's reload. `true` by
    /// default, because a rollback onto a release that runs is a new
    /// process under a new pid.
    rollback_replaces: bool,
    /// How many `describe` calls after the rollback's reload it takes the
    /// restored release to appear, standing in for a previous release that
    /// needs longer to become ready than the one rejected did.
    rollback_turnover_after: u32,
    /// The `describe` count when the rollback's reload was accepted.
    second_reload_at: Cell<u32>,
    /// From which reload count `describe` answers `NotFound`, standing in
    /// for a shepherd that dropped the sheep from the flock because it
    /// could not spawn it. `None` for never.
    drops_the_sheep_from: Option<u32>,
    /// From which reload count `describe` starts failing outright, or
    /// `None` for never.
    ///
    /// `Some(1)` is a shepherd that answers the generation capture and
    /// then stops: an error arriving after the reload has already
    /// drained the old instance, which is a different path from a sheep
    /// that will not come up. `Some(0)` fails the capture itself, which
    /// is the step that used to sit outside the rollback boundary.
    describe_fails_from: Option<u32>,
    /// Created just before answering the first `describe`, standing in
    /// for something that is not this dog's leaving a file at the
    /// `current.tmp` path at the worst possible moment - after the swap,
    /// before the rollback. A regular file, because a stale symlink there
    /// is this dog's own leftover and the swap replaces it.
    plant: Option<PathBuf>,
    /// A record to set `watch = manual` in when the first reload
    /// arrives, standing in for an operator running `--watch manual`
    /// while the deploy is past its build and inside its reload.
    flip_watch: Option<PathBuf>,
    /// How many reloads after the first to refuse, standing in for the
    /// shepherd's own refusal to start a reload while one is in flight.
    /// The first is never refused, because that is the deploy's own and
    /// the case being modelled is a ROLLBACK arriving while it runs.
    refusals: Cell<u32>,
    /// The code those refusals carry. `Internal` is what a real
    /// `ReloadInFlight` arrives as; `NotFound` is the one that can
    /// never clear.
    refusal_code: RpcErrorCode,
    /// Whether the deploy's OWN reload is refused too, rather than only
    /// the rollback's.
    refuse_from_the_first: Cell<bool>,
    /// How many reloads LAND and then answer a transport error - the
    /// reply lost rather than the request refused. The swap goes ahead
    /// each time, so the flock turns over exactly as it would have; all
    /// the caller learns is that no answer came back. `u32::MAX` never
    /// stops losing them.
    replies_lost: Cell<u32>,
    /// Every `reload` call, refused ones included - which is how a test
    /// can see whether something was retried.
    attempts: Cell<u32>,
    /// Whether the pid moves on every `describe`, standing in for a
    /// release that comes up and then keeps dying and being restarted.
    flapping: bool,
    /// How many instances the flock is running. The whole point of the
    /// number: a reload costs one swap per RUNNING instance, whatever
    /// the Flockfile happens to ask for.
    instances: u32,
    /// How many `describe` calls the replacement takes to appear,
    /// standing in for a reload that is genuinely still in flight. `0`
    /// is the instantaneous swap every other test wants.
    turnover_after: u32,
    /// Counted only so [`Self::flapping`] has something to move with.
    describes: Cell<u32>,
    reloads: Cell<u32>,
    /// The sheep's `[app.dogs.deploy]` table, in TOML, or `None` for a sheep
    /// registered without one.
    settings: Option<String>,
}

/// The pid serving before any reload. Any other value would do; this
/// one is only recognisable in a failure message.
const FIRST_PID: u32 = 12835;

impl Shepherd {
    /// A reload that replaces the process and brings it to `Online`.
    pub(crate) fn ready() -> Self {
        Self {
            settles_to: ProcStatus::Online,
            replaces: true,
            rollback_settles_to: ProcStatus::Online,
            rollback_replaces: true,
            rollback_turnover_after: 0,
            second_reload_at: Cell::new(0),
            drops_the_sheep_from: None,
            describe_fails_from: None,
            plant: None,
            flip_watch: None,
            refusals: Cell::new(0),
            refusal_code: RpcErrorCode::Internal,
            refuse_from_the_first: Cell::new(false),
            replies_lost: Cell::new(0),
            attempts: Cell::new(0),
            flapping: false,
            instances: 1,
            turnover_after: 0,
            describes: Cell::new(0),
            reloads: Cell::new(0),
            settings: None,
        }
    }

    /// The same shepherd, holding `settings` as the sheep's
    /// `[app.dogs.deploy]` table.
    pub(crate) fn with_settings(self, settings: &str) -> Self {
        Self {
            settings: Some(settings.to_owned()),
            ..self
        }
    }

    /// A reload that replaces the process with one that never becomes
    /// ready.
    pub(crate) fn never_ready() -> Self {
        Self {
            settles_to: ProcStatus::Starting,
            ..Self::ready()
        }
    }

    /// A reload shep accepted and then gave up on, keeping the old
    /// instance serving: one `Online` entry, under the pid that was
    /// already there. Measured against a real shepherd - see
    /// `crate::verify`'s module doc for the transcript.
    pub(super) fn keeps_the_old_instance() -> Self {
        Self {
            replaces: false,
            ..Self::ready()
        }
    }

    /// As [`Self::never_ready`], and the rollback's replacement spawns and
    /// never becomes ready either.
    pub(super) fn rollback_never_ready() -> Self {
        Self {
            rollback_settles_to: ProcStatus::Starting,
            ..Self::never_ready()
        }
    }

    /// As [`Self::never_ready`], and once the rollback's reload has been
    /// accepted the shepherd no longer knows the sheep: `describe`
    /// answers `NotFound`. What shep does when the release it is asked to
    /// go back to cannot be spawned - a binary that is a link to a deleted
    /// file, say.
    pub(super) fn rollback_drops_the_sheep() -> Self {
        Self {
            drops_the_sheep_from: Some(2),
            ..Self::never_ready()
        }
    }

    /// A shepherd whose reloads never turn the flock over: both the
    /// deploy's and the rollback's leave the old instance, under its old
    /// pid.
    pub(super) fn never_turns_over() -> Self {
        Self {
            replaces: false,
            rollback_replaces: false,
            ..Self::ready()
        }
    }

    /// Answers the generation capture and then fails every `describe`
    /// after the reload - the transient error `verify::wait`'s own doc
    /// anticipates.
    pub(super) fn describe_fails() -> Self {
        Self {
            describe_fails_from: Some(1),
            ..Self::ready()
        }
    }

    /// Fails every `describe`, starting with the generation capture
    /// itself. A real shepherd does neither on command, which is why
    /// the only place this can be exercised is here.
    pub(super) fn unreachable() -> Self {
        Self {
            describe_fails_from: Some(0),
            ..Self::ready()
        }
    }

    /// A shepherd too busy to accept even the deploy's own reload -
    /// an operator's `shep reload web` landing first, which shep
    /// refuses for as long as it is in flight.
    pub(super) fn too_busy_to_start() -> Self {
        let shepherd = Self::ready();
        shepherd.refusals.set(u32::MAX);
        shepherd.refuse_from_the_first.set(true);
        shepherd
    }

    /// As [`Self::ready`], but sets `watch = manual` in the record at
    /// `record` when the reload arrives, as an operator would mid-deploy.
    pub(super) fn flipping_watch(record: PathBuf) -> Self {
        Self {
            flip_watch: Some(record),
            ..Self::ready()
        }
    }

    /// As [`Self::never_ready`], but leaves a file in the way at `plant`
    /// (the `current.tmp` path) on the way past, so the rollback's swap
    /// back cannot land.
    pub(super) fn planting(plant: PathBuf) -> Self {
        Self {
            plant: Some(plant),
            ..Self::never_ready()
        }
    }

    /// A shepherd that is busy with the deploy's own reload when the
    /// rollback's arrives, and refuses the next `times` of them.
    /// `u32::MAX` never stops refusing.
    pub(super) fn busy_for(times: u32) -> Self {
        let shepherd = Self::never_ready();
        shepherd.refusals.set(times);
        shepherd
    }

    /// A release that comes up and then will not stay up: a fresh pid
    /// on every look, which is what a crash loop looks like from
    /// `describe`.
    pub(super) fn flapping() -> Self {
        Self {
            flapping: true,
            ..Self::ready()
        }
    }

    /// A reload that succeeds, but only once `describes` more looks
    /// have gone by - a swap that is genuinely still in flight rather
    /// than one that failed. At `verify`'s 100ms poll, `describes` is
    /// tenths of a second of budget.
    pub(super) fn ready_after(describes: u32) -> Self {
        Self {
            turnover_after: describes,
            ..Self::ready()
        }
    }

    /// The same shepherd, with a rollback that comes up only once `describes`
    /// more looks have gone by after its reload. At `verify`'s 100ms poll,
    /// `describes` is tenths of a second.
    pub(super) fn rollback_ready_after(self, describes: u32) -> Self {
        Self {
            rollback_turnover_after: describes,
            ..self
        }
    }

    /// The same shepherd, running `instances` instances.
    pub(super) fn running(self, instances: u32) -> Self {
        Self { instances, ..self }
    }

    /// A shepherd whose next `count` reloads are accepted and acted on
    /// with their replies never coming back: `Timeout` after the swap
    /// has already begun.
    ///
    /// The window is real rather than theoretical - `DEFAULT_DEADLINE`
    /// is five seconds and `DEADLINE_GRACE` two, and a reload of
    /// several instances outlives both by design, which is why shep
    /// answers it as an acceptance in the first place.
    pub(super) fn losing_replies(count: u32) -> Self {
        let shepherd = Self::ready();
        shepherd.replies_lost.set(count);
        shepherd
    }

    /// A shepherd that has never heard of this sheep: every reload
    /// after the first is `NotFound`, which no amount of asking again
    /// can change.
    pub(super) fn unregistered() -> Self {
        Self {
            refusal_code: RpcErrorCode::NotFound,
            ..Self::busy_for(u32::MAX)
        }
    }

    pub(super) fn attempt_count(&self) -> u32 {
        self.attempts.get()
    }

    pub(crate) fn reload_count(&self) -> u32 {
        self.reloads.get()
    }

    /// How many reloads have actually landed. A reload that has been
    /// accepted but whose replacement has not appeared yet counts for
    /// nothing, which is what an in-flight swap looks like from
    /// `describe`: the old generation, still serving.
    fn landed(&self) -> u32 {
        if self.describes.get() <= self.turnover_after {
            0
        } else if self.reloads.get() >= 2 && !self.rollback_landed() {
            1
        } else {
            self.reloads.get()
        }
    }

    /// Whether the rollback's reload, once accepted, has put its replacement
    /// in place.
    fn rollback_landed(&self) -> bool {
        self.reloads.get() >= 2
            && self.describes.get() > self.second_reload_at.get() + self.rollback_turnover_after
    }

    /// The pid of instance `index`: a fresh generation per landed
    /// reload, unless this shepherd keeps the old instance.
    fn pid(&self, index: u32) -> u32 {
        if self.rolling_back() {
            if !self.rollback_replaces {
                return FIRST_PID + index;
            }
            // Past every pid the deploy's own reload can have shown, and
            // steady: a rollback that comes up stays up.
            return FIRST_PID + 1_000_000 + index;
        }
        if !self.replaces {
            return FIRST_PID + index;
        }
        let churn = if self.flapping {
            self.describes.get()
        } else {
            0
        };
        FIRST_PID + (self.landed() + churn) * 100 + index
    }

    /// Whether the rollback's reload, the second, has landed.
    fn rolling_back(&self) -> bool {
        self.rollback_landed() && self.landed() > 0
    }

    /// The status currently serving: whatever this shepherd settles to
    /// once a reload has landed, and `Online` before that, since the
    /// process that was already there is up.
    fn status(&self) -> ProcStatus {
        if self.landed() == 0 {
            ProcStatus::Online
        } else if self.rolling_back() {
            self.rollback_settles_to
        } else {
            self.settles_to
        }
    }
}

impl Daemon for Shepherd {
    async fn sheep_config(&self, sheep: &str) -> Result<AppConfig, Error> {
        Ok(match &self.settings {
            Some(settings) => fixtures::app_with_settings(sheep, settings),
            None => fixtures::app_named(sheep),
        })
    }
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        // `exists()` follows the link, and this one dangles on purpose,
        // so ask about the link itself or every poll plants it again.
        if let Some(plant) = &self.plant
            && plant.symlink_metadata().is_err()
        {
            // A regular file, not a symlink: a stale tmp LINK is this
            // dog's own leftover and `swap::point_at` replaces it, so
            // only something that is not a link still refuses the swap.
            std::fs::write(plant, b"in the way").expect("plant something at the tmp path");
        }
        self.describes.set(self.describes.get() + 1);
        if self
            .describe_fails_from
            .is_some_and(|from| self.reloads.get() >= from)
        {
            return Err(Error::Protocol("the shepherd stopped answering".to_owned()));
        }
        if self
            .drops_the_sheep_from
            .is_some_and(|from| self.reloads.get() >= from)
        {
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: RpcErrorCode::NotFound,
                message: format!("no sheep named {sheep}"),
                daemon_version: None,
            })));
        }
        Ok((0..self.instances)
            .map(|index| {
                ProcessInfo::builder(index, sheep, self.status())
                    .pid(Some(self.pid(index)))
                    .build()
            })
            .collect())
    }
    async fn reload(&self, sheep: &str) -> Result<(), Error> {
        if let Some(record) = &self.flip_watch
            && self.attempts.get() == 0
        {
            let mut state = State::read(record).expect("the record to flip");
            state.watch = Watch::Manual;
            state
                .write(record)
                .expect("flip watch to manual mid-deploy");
        }
        self.attempts.set(self.attempts.get() + 1);
        let refusals = self.refusals.get();
        if (self.reloads.get() > 0 || self.refuse_from_the_first.get()) && refusals > 0 {
            if refusals != u32::MAX {
                self.refusals.set(refusals - 1);
            }
            // The shape shep really answers with: `ReloadInFlight` has
            // no code of its own, so it arrives as `Internal`. A fake
            // that answered `Protocol` here would let a retry policy
            // that cannot read codes look correct.
            return Err(Error::Request(RequestError::Rpc(RpcError {
                code: self.refusal_code,
                message: format!("{sheep} is already being reloaded"),
                daemon_version: None,
            })));
        }
        self.reloads.set(self.reloads.get() + 1);
        if self.reloads.get() == 2 {
            self.second_reload_at.set(self.describes.get());
        }

        let lost = self.replies_lost.get();
        if lost > 0 {
            if lost != u32::MAX {
                self.replies_lost.set(lost - 1);
            }
            return Err(Error::Request(RequestError::Timeout {
                after: Duration::from_secs(7),
            }));
        }
        Ok(())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, restart, save_roll, set_smit,
    );
}
