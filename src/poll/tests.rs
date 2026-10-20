//! Tests for the poll loop: ticks, config refresh and the loop itself.

use crate::fixtures;

use super::*;

mod smit;

/// fails if a target the dog cannot name is skipped, keyed by a lossy
/// name, or printed with the character that made it unnameable. The
/// key is the path's `Debug` form so two such directories never share a
/// mute entry; the line an operator reads has the newline replaced.
#[tokio::test]
async fn an_unnameable_target_is_a_row_that_cannot_forge_a_line() {
    let home = tempfile::tempdir().expect("tempdir");
    let dir = home.path().join("deploy").join("bad\nname");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("deploy.toml"), "").expect("record");

    let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
        .await
        .results;

    assert_eq!(results.len(), 1, "{results:?}");
    let (key, outcome) = &results[0];
    assert_eq!(key, &format!("{dir:?}"));
    let said = report(key, outcome).expect("a complaint");
    assert!(!said.text().contains('\n'), "{:?}", said.text());
    assert!(
        said.text().contains("rename the directory"),
        "{}",
        said.text()
    );
}

use core::time::Duration;
use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use shep_client::RequestError;
use shep_client::shep_core::config::AppConfig;
use shep_client::shep_core::protocol::{ProcessInfo, RpcError, RpcErrorCode};
use shep_client::shep_core::status::ProcStatus;
use tempfile::TempDir;

use crate::state::Verify;
use crate::swap;

/// The fixture app, named per sheep so `flockfile::app_config` finds
/// it, and probed because `Verify::Probed` refuses a sheep whose
/// readiness nothing gates.
fn flockfile(sheep: &str) -> String {
    format!(
        "[[app]]\nname = '{sheep}'\nscript = './run.sh'\n\n\
         [app.readiness_probe]\nkind = 'http'\ntarget = 'http://127.0.0.1:1/health'\n"
    )
}

/// A target's record, for the one function that reads nothing else.
fn target(watch: Watch) -> State {
    State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        watch,
        ..fixtures::state()
    }
}

/// Writes `<home>/deploy/<sheep>/deploy.toml` and nothing else: a
/// target with a record and no repository under it at all.
fn write_target(home: &Path, sheep: &str, watch: Watch, sha: Option<&str>) {
    let tree = Tree::for_sheep(home, sheep);
    fs::create_dir_all(tree.root()).expect("create the tree");
    let mut state = target(watch);
    state.deployed = sha.map(str::to_owned);
    state.write(&tree.state_file()).expect("write deploy.toml");
}

/// A target that a tick can really deploy: an origin repository one
/// commit ahead, a bare clone, a release already live under `current`,
/// and a record naming it.
///
/// The returned [`TempDir`] is the origin, and dropping it deletes the
/// repository the deploy fetches from, so tests hold it.
fn write_target_ready(home: &Path, sheep: &str, watch: Watch) -> TempDir {
    let origin = fixtures::checkout(&[("Flockfile.toml", &flockfile(sheep))]);

    let tree = Tree::for_sheep(home, sheep);
    fs::create_dir_all(tree.git()).expect("create the git dir");
    fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);
    let remote = origin.path().to_str().expect("utf-8 path").to_owned();
    crate::git::fetch(&tree.git(), &remote, fixtures::TEST_BUDGET).expect("fetch");

    let first = crate::git::remote_head(&tree.git(), "main").expect("head");
    crate::git::worktree_add(&tree.git(), &tree.release(&first), &first).expect("worktree");
    swap::point_at(&tree.current(), &tree.release(&first)).expect("swap");

    let state = State {
        remote,
        deployed: Some(first),
        verify: Verify::Probed,
        watch,
        checkout: origin.path().to_owned(),
        ..fixtures::state()
    };
    state.write(&tree.state_file()).expect("write deploy.toml");

    // The commit the tick has to notice. Without it every deploy is
    // `UpToDate` and the tests below would pass on a loop that does
    // nothing at all.
    fs::write(origin.path().join("second.txt"), "x").expect("write");
    fixtures::run_git(origin.path(), &["add", "."]);
    fixtures::run_git(origin.path(), &["commit", "-q", "-m", "second"]);

    origin
}

/// The pid serving before any reload. Any other value would do; this
/// one is only recognisable in a failure message.
const FIRST_PID: u32 = 12835;

/// A shepherd whose reload really replaces the process, so a deploy
/// against a sound tree verifies and lands.
///
/// The pid moves on the RELOAD rather than on every `describe`.
/// A double whose pid moved on its own would report a turnover to any
/// caller that looked twice, which is how a deploy that never reloaded
/// anything could pass for one that did.
struct Ready {
    reloads: Cell<u32>,
}

impl Ready {
    const fn new() -> Self {
        Self {
            reloads: Cell::new(0),
        }
    }
}

impl Daemon for Ready {
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        Ok(vec![
            ProcessInfo::builder(0, sheep, ProcStatus::Online)
                .pid(Some(FIRST_PID + self.reloads.get() * 100))
                .build(),
        ])
    }
    async fn reload(&self, _sheep: &str) -> Result<(), Error> {
        self.reloads.set(self.reloads.get() + 1);
        Ok(())
    }
    async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
        Ok(())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, restart, save_roll,
    );
}

/// A shepherd that has gone quiet, counting the times it was asked, on
/// a branch that moves while it is down.
///
/// One `describe` per tick, exactly. A deploy captures the generation
/// before its reload, that capture is the first thing it asks for, and
/// this answers it with an error, so the deploy puts `current` back and
/// gives up before asking anything else.
///
/// The commit is what makes the next tick do the same thing again
/// rather than hold: an attempt that failed is recorded against its
/// sha, and the branch moving is what clears it. So this is a target
/// somebody keeps pushing to while the shepherd is down, which is the
/// one shape that really does deploy on every tick - and the reason the
/// count is a tick count.
///
/// A count far above what the interval allows means the loop stopped
/// sleeping. That is a busy loop with no await point in it, and under a
/// paused clock it would hang the test rather than fail it, because the
/// runtime never parks and so never advances time to the timeout. The
/// panic turns that hang into a red test with a reason on it.
struct Counting {
    describes: Cell<u32>,
    origin: PathBuf,
}

impl Counting {
    fn new(origin: &Path) -> Self {
        Self {
            describes: Cell::new(0),
            origin: origin.to_owned(),
        }
    }

    fn ticks(&self) -> u32 {
        self.describes.get()
    }
}

impl Daemon for Counting {
    async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        let asked = self.describes.get() + 1;
        self.describes.set(asked);
        assert!(
            asked <= 60,
            "the loop ticked far more than the interval allows: it is not sleeping"
        );
        fs::write(self.origin.join(format!("{asked}.txt")), "x").expect("write");
        fixtures::run_git(&self.origin, &["add", "."]);
        fixtures::run_git(&self.origin, &["commit", "-q", "-m", "another"]);
        Err(Error::Protocol("the shepherd stopped answering".to_owned()))
    }
    async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
        Ok(())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, reload, restart, save_roll,
    );
}

/// fails if a manual target is polled. This is the switch an operator
/// reaches for during an incident, and a deploy firing at that moment
/// is the exact opposite of what they asked for. `set_watch` already
/// has a test that SETTING the mode does not deploy; this is the other
/// half, that HOLDING it does not either.
#[tokio::test]
async fn a_manual_target_is_never_polled() {
    assert!(!due(&target(Watch::Manual)));
    assert!(due(&target(Watch::Auto)));
}

/// fails if a manual target is deployed by a tick that runs over it.
/// The line above is the decision on its own; this is the decision
/// being obeyed, and it is the one that would let a paused target
/// deploy in the middle of an incident.
#[tokio::test(start_paused = true)]
async fn a_manual_target_is_left_alone_by_a_whole_tick() {
    let home = tempfile::tempdir().expect("tempdir");
    let _origin = write_target_ready(home.path(), "paused", Watch::Manual);
    let before = State::read(&Tree::for_sheep(home.path(), "paused").state_file())
        .expect("reads")
        .deployed;

    assert!(
        tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results
            .is_empty()
    );

    assert_eq!(
        State::read(&Tree::for_sheep(home.path(), "paused").state_file())
            .expect("reads")
            .deployed,
        before,
        "still on the release it was on"
    );
}

/// fails if one target's failure stops the tick. A dog that gives up
/// because one of five remotes was unreachable stops deploying the
/// other four, and nothing restarts it with a reason anybody can read.
#[tokio::test(start_paused = true)]
async fn one_targets_failure_does_not_stop_the_others() {
    let home = tempfile::tempdir().expect("tempdir");
    // A record naming a release, with no repository under it at all:
    // the fetch is what fails, which is the shape of a remote nobody
    // can reach.
    write_target(home.path(), "broken", Watch::Auto, Some(fixtures::SHA));
    let _origin = write_target_ready(home.path(), "fine", Watch::Auto);

    let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
        .await
        .results;

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "broken");
    assert!(results[0].1.is_err(), "broken failed");
    assert_eq!(results[1].0, "fine");
    assert!(
        matches!(results[1].1, Ok(Outcome::Deployed { .. })),
        "fine still ran: {:?}",
        results[1].1
    );
}

/// fails if a tick rebuilds a sha the last attempt failed on. This is
/// the difference between the loop's entry into the deploy sequence and
/// an operator's: unheld, one bad commit costs two reloads of a live
/// app and a full rebuild every interval, indefinitely, and nothing
/// about that self-corrects until somebody pushes.
#[tokio::test(start_paused = true)]
async fn a_tick_holds_a_sha_that_already_failed() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = write_target_ready(home.path(), "held", Watch::Auto);
    let tree = Tree::for_sheep(home.path(), "held");
    let mut state = State::read(&tree.state_file()).expect("reads");
    state.failed = Some(fixtures::head_of(origin.path()));
    state.write(&tree.state_file()).expect("writes");

    let daemon = Ready::new();
    let results = tick(&daemon, home.path(), &fixtures::dog_config())
        .await
        .results;

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0].1, Err(Error::Held { .. })),
        "{:?}",
        results[0].1
    );
    assert_eq!(daemon.reloads.get(), 0, "nothing was reloaded");
}

/// fails if a tick with no targets at all is an error. That is every
/// freshly adopted dog, and it must idle quietly rather than logging a
/// failure every thirty seconds forever.
#[tokio::test]
async fn a_dog_with_no_targets_ticks_quietly() {
    let home = tempfile::tempdir().expect("tempdir");
    assert!(
        tick(&Ready::new(), home.path(), &fixtures::dog_config())
            .await
            .results
            .is_empty()
    );
}

/// fails if a deploy directory that cannot be listed is passed over in
/// silence. Every target is under it, so the dog has stopped deploying
/// everything - and idling quietly is what a dog with nothing to do
/// looks like too. One row, named for the directory rather than for a
/// sheep, because there is no sheep to name.
///
/// Relies on mode bits, which root ignores: run as root (a container,
/// say) the listing succeeds and this test fails for a reason that has
/// nothing to do with the loop. CI runs unprivileged.
#[tokio::test]
async fn a_deploy_directory_that_cannot_be_listed_is_reported() {
    let home = tempfile::tempdir().expect("tempdir");
    let root = home.path().join("deploy");
    fs::create_dir_all(&root).expect("create the deploy dir");
    // Listable again on drop or not, this test never reads it back.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("chmod");

    let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
        .await
        .results;

    let listable = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, root.display().to_string());
    assert!(results[0].1.is_err());
    listable.expect("chmod back");
}

/// fails if a record that cannot be read is skipped in silence. A
/// `deploy.toml` that will not parse is a target the dog can do nothing
/// with and nobody will hear about, and `paths::targets` only lists
/// directories that have one - so this is a broken record rather than
/// an ordinary absence.
#[tokio::test]
async fn a_record_that_cannot_be_read_is_reported_rather_than_skipped() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "garbled");
    fs::create_dir_all(tree.root()).expect("create the tree");
    fs::write(tree.state_file(), "this is not toml").expect("write deploy.toml");

    let results = tick(&Ready::new(), home.path(), &fixtures::dog_config())
        .await
        .results;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "garbled");
    assert!(results[0].1.is_err());
}

/// fails if the loop stops sleeping the configured interval between
/// ticks. A loop that polls continuously fetches continuously, which
/// hammers every remote it watches and reads as a hung dog.
///
/// `start_paused` so a five-minute interval costs no wall clock: the
/// same idiom the deploy tests use to run out a real verification
/// budget instantly. The target is a real one rather than an empty
/// home, because a tick over an empty home asks the shepherd nothing
/// and there would be nothing to count.
#[tokio::test(start_paused = true)]
async fn ticks_are_spaced_by_the_configured_interval() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = write_target_ready(home.path(), "quiet", Watch::Auto);
    let counter = Counting::new(origin.path());
    let began = tokio::time::Instant::now();

    // Three ticks' worth, then stop.
    let _ = tokio::time::timeout(
        Duration::from_secs(305),
        run(
            &counter,
            home.path(),
            Reader::primed(
                None,
                DogConfig {
                    interval: Duration::from_secs(150),
                    ..fixtures::dog_config()
                },
            ),
        ),
    )
    .await;

    assert_eq!(counter.ticks(), 3, "one at t=0, then every 150s");
    assert!(began.elapsed() >= Duration::from_secs(300));
}

/// fails if the first tick waits for the interval before running. A dog
/// that polls nothing for the first thirty seconds after every restart
/// looks broken for thirty seconds after every restart, and a longer
/// interval makes it worse in proportion.
#[tokio::test(start_paused = true)]
async fn the_first_tick_happens_at_once() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = write_target_ready(home.path(), "quiet", Watch::Auto);
    let counter = Counting::new(origin.path());

    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        run(
            &counter,
            home.path(),
            Reader::primed(
                None,
                DogConfig {
                    interval: Duration::from_secs(600),
                    ..fixtures::dog_config()
                },
            ),
        ),
    )
    .await;

    assert_eq!(counter.ticks(), 1);
}

/// fails if a deploy the loop made never reaches the log. The whole
/// output block could be deleted and the tests in `report.rs` still pass:
/// `report` and `worth_saying` are pure, and nothing else pins that
/// their verdict is what actually gets written, or that it goes to
/// stdout where an operator looks for what shipped.
#[tokio::test(start_paused = true)]
async fn a_deploy_is_written_to_the_log_it_belongs_in() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = write_target_ready(home.path(), "fine", Watch::Auto);
    let head = fixtures::head_of(origin.path());
    let (mut out, mut err) = (Vec::new(), Vec::new());

    let _ = tokio::time::timeout(
        // Paused time: long enough for a verify dwell, over in an instant.
        Duration::from_secs(120),
        run_with(
            &Ready::new(),
            home.path(),
            Reader::primed(
                None,
                DogConfig {
                    interval: Duration::from_secs(600),
                    ..fixtures::dog_config()
                },
            ),
            &mut out,
            &mut err,
        ),
    )
    .await;

    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("fine deployed {head}\n")
    );
    assert!(err.is_empty(), "nothing failed");
}

/// fails if a failure never reaches the error log, or reaches it once
/// per tick. Both halves matter and neither is pinned by the pure
/// tests: this is the only thing that says `worth_saying`'s verdict
/// actually gates a write.
#[tokio::test(start_paused = true)]
async fn a_failure_is_written_once_however_many_ticks_repeat_it() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target(home.path(), "broken", Watch::Auto, Some(fixtures::SHA));
    let (mut out, mut err) = (Vec::new(), Vec::new());

    // Three ticks' worth, as the interval test above.
    let _ = tokio::time::timeout(
        Duration::from_secs(305),
        run_with(
            &Ready::new(),
            home.path(),
            Reader::primed(
                None,
                DogConfig {
                    interval: Duration::from_secs(150),
                    ..fixtures::dog_config()
                },
            ),
            &mut out,
            &mut err,
        ),
    )
    .await;

    let complained = String::from_utf8(err).expect("utf-8");
    assert_eq!(complained.lines().count(), 1, "{complained}");
    assert!(complained.starts_with("broken: "), "{complained}");
    assert!(out.is_empty(), "nothing deployed");
}

/// The name a test's shepherd has adopted this dog under. Any name
/// would do; [`fixtures::Sections`] answers whatever it is asked.
const ADOPTED: &str = "deploy";

/// A reader that will really ask for its section, starting from the
/// fixture config so that a test's own sections are the only values it
/// could be running on afterwards.
fn reading() -> Reader {
    Reader::primed(Some(ADOPTED.to_owned()), fixtures::dog_config())
}

/// fails if a change to this dog's own section never reaches a running
/// dog. `shep lookout` writes the section and then tells the operator
/// the dog has been told, so a dog that goes on polling at the interval
/// it started with makes that sentence false with nothing anywhere
/// saying so - and the operator only finds out by timing the deploys.
///
/// The second section is SHORTER than the first, so the count can only
/// come out right if the new value is what the loop slept on: at ten
/// minutes it is asked at t=0 and t=600, at one minute it is asked
/// again at t=660, and the tick after that falls outside the timeout.
/// A dog that re-read the section and kept using the old one asks
/// twice; one still on the fixture's thirty seconds asks twenty-four
/// times.
#[tokio::test(start_paused = true)]
async fn a_changed_interval_reaches_the_dog_without_a_restart() {
    let home = fixtures::tempdir();
    let daemon = fixtures::Sections::of("interval = \"600s\"", &["interval = \"60s\""]);
    let (mut out, mut err) = (Vec::new(), Vec::new());

    let _ = tokio::time::timeout(
        Duration::from_secs(700),
        run_with(&daemon, home.path(), reading(), &mut out, &mut err),
    )
    .await;

    assert_eq!(daemon.asked(), 3, "t=0 at 600s, t=600 at 60s, t=660");
}

/// fails if a section that stops parsing mid-run takes the config the
/// dog was working on with it, or takes the dog down.
///
/// Both would be defensible on their own and neither is what this dog
/// does: a tick here can be a whole deploy, and the section that parsed
/// a minute ago is one the operator asked for rather than a default. So
/// the refused section leaves the interval where it was - asked at
/// t=0, 60, 120 and 180, where the thirty-second default would have
/// asked six times and an exit would have asked twice.
///
/// A request the shepherd refuses takes the same branch; this is the
/// half an operator can cause with a text editor.
#[tokio::test(start_paused = true)]
async fn a_section_that_stops_parsing_keeps_the_one_that_worked() {
    let home = fixtures::tempdir();
    let daemon = fixtures::Sections::of("interval = \"60s\"", &["retention = 1"]);
    let (mut out, mut err) = (Vec::new(), Vec::new());

    let _ = tokio::time::timeout(
        Duration::from_secs(200),
        run_with(&daemon, home.path(), reading(), &mut out, &mut err),
    )
    .await;

    assert_eq!(daemon.asked(), 4, "still a minute apart");

    // The empty home complains about having no targets on every tick
    // too, and that row is muted the same way; this one is the row
    // keyed on the file an operator has to edit.
    let complained = String::from_utf8(err).expect("utf-8");
    let about_the_section: Vec<&str> = complained
        .lines()
        .filter(|line| line.contains("dogs.toml"))
        .collect();
    assert_eq!(about_the_section.len(), 1, "said once, not per tick");
    let line = about_the_section[0];
    assert!(line.contains("keeps too few releases"), "{line}");
    assert!(
        line.contains("still polling on the section it last read"),
        "{line}"
    );
}
