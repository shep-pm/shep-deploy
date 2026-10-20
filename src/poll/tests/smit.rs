//! Tests for the smit each tick republishes.

use super::*;

/// A [`Daemon`] whose `set_smit` records every `(sheep, text)` it was
/// asked to paint, in call order, and which otherwise behaves as
/// [`Ready`] does.
struct SmitRecording {
    ready: Ready,
    smits: RefCell<Vec<(String, String)>>,
}

impl Default for SmitRecording {
    fn default() -> Self {
        Self {
            ready: Ready::new(),
            smits: RefCell::new(Vec::new()),
        }
    }
}

impl SmitRecording {
    fn smits(&self) -> Vec<(String, String)> {
        self.smits.borrow().clone()
    }
}

impl Daemon for SmitRecording {
    async fn dog_config(&self, name: &str) -> Result<String, Error> {
        self.ready.dog_config(name).await
    }
    async fn list_flock(&self) -> Result<Vec<ProcessInfo>, Error> {
        self.ready.list_flock().await
    }
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        self.ready.describe(sheep).await
    }
    async fn start(&self, apps: Vec<AppConfig>) -> Result<Vec<u32>, Error> {
        self.ready.start(apps).await
    }
    async fn delete(&self, id: u32) -> Result<(), Error> {
        self.ready.delete(id).await
    }
    async fn reload(&self, sheep: &str) -> Result<(), Error> {
        self.ready.reload(sheep).await
    }
    async fn restart(&self, sheep: &str) -> Result<(), Error> {
        self.ready.restart(sheep).await
    }
    async fn save_roll(&self) -> Result<PathBuf, Error> {
        self.ready.save_roll().await
    }
    async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> {
        self.smits
            .borrow_mut()
            .push((sheep.to_owned(), text.to_owned()));
        Ok(())
    }
}

/// A [`Daemon`] whose `set_smit` always answers an [`RpcError`], and
/// which otherwise deploys cleanly - the shepherd refusing a smit is
/// not the same thing as a shepherd that cannot be reached.
struct RefusingSmits(Ready);

impl Daemon for RefusingSmits {
    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        self.0.describe(sheep).await
    }
    async fn reload(&self, sheep: &str) -> Result<(), Error> {
        self.0.reload(sheep).await
    }
    async fn set_smit(&self, _sheep: &str, _text: &str) -> Result<(), Error> {
        Err(Error::Request(RequestError::Rpc(RpcError {
            code: RpcErrorCode::Internal,
            message: "smits are not accepted right now".to_owned(),
            daemon_version: None,
        })))
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, restart, save_roll,
    );
}

/// fails if the smit stops being republished every tick. The daemon
/// holds smits in memory and drops them when the dog stops, so a dog
/// that published once and then only on change would show nothing at
/// all after a daemon restart until the next deploy, which for a
/// healthy target could be weeks.
#[tokio::test]
async fn every_tick_republishes_every_targets_smit() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target(
        home.path(),
        "bpm",
        Watch::Auto,
        Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
    );
    write_target(
        home.path(),
        "ctm",
        Watch::Manual,
        Some("f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"),
    );
    let daemon = SmitRecording::default();

    tick(&daemon, home.path(), &fixtures::dog_config()).await;
    tick(&daemon, home.path(), &fixtures::dog_config()).await;

    assert_eq!(
        daemon.smits(),
        vec![
            ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
            ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
            ("bpm".to_owned(), "▲ main@a1b2c3".to_owned()),
            ("ctm".to_owned(), "⏸ main@f6e5d4".to_owned()),
        ]
    );
}

/// fails if a MANUAL target loses its smit. `due` skips manual targets
/// for deploying, and the smit is exactly how an operator sees that a
/// target is paused, so skipping the smit too would hide the state the
/// smit exists to show.
#[tokio::test]
async fn a_manual_target_still_gets_a_smit() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target(
        home.path(),
        "ctm",
        Watch::Manual,
        Some("f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"),
    );
    let daemon = SmitRecording::default();
    tick(&daemon, home.path(), &fixtures::dog_config()).await;
    assert_eq!(daemon.smits().len(), 1);
}

/// fails if a smit that could not be published takes the tick down with
/// it. A daemon that refuses one is a daemon this dog can still deploy
/// through, and a cosmetic call must never cost a deploy.
#[tokio::test]
async fn a_refused_smit_does_not_stop_the_tick() {
    let home = tempfile::tempdir().expect("tempdir");
    // Held, unlike the brief's own listing of this test: an unheld
    // `TempDir` drops (and deletes) the origin the instant this line
    // ends, which fails the fetch for a reason that has nothing to do
    // with the smit refusal this test exists to pin - see every other
    // `write_target_ready` caller in `tests.rs` for the pattern.
    let _origin = write_target_ready(home.path(), "fine", Watch::Auto);
    let results = tick(
        &RefusingSmits(Ready::new()),
        home.path(),
        &fixtures::dog_config(),
    )
    .await
    .results;
    let row = |name: &str| {
        results
            .iter()
            .find(|(sheep, _)| sheep == name)
            .unwrap_or_else(|| panic!("no row for {name}: {results:?}"))
    };
    assert!(row("fine").1.is_ok(), "the deploy still ran");
    // And the refusal is a row of its own, so it reaches the mute rather
    // than being printed once per target per tick forever.
    assert!(
        row("fine/smit").1.is_err(),
        "the refusal is reported as its own row"
    );
}

/// fails if a target whose branch name is longer than a smit allows
/// takes the deploy down with it, or simply publishes nothing. The
/// text is shortened by `smit::publish` before it ever reaches the
/// daemon, so this reaches the daemon at all rather than erroring out
/// of `set_smit` locally.
#[tokio::test]
async fn a_too_long_branch_name_does_not_stop_the_tick() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target(
        home.path(),
        "long",
        Watch::Auto,
        Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
    );
    let tree = Tree::for_sheep(home.path(), "long");
    let mut state = State::read(&tree.state_file()).expect("reads");
    state.branch = "x".repeat(100);
    state.write(&tree.state_file()).expect("writes");

    let daemon = SmitRecording::default();
    let results = tick(&daemon, home.path(), &fixtures::dog_config())
        .await
        .results;

    assert!(
        !daemon.smits().is_empty(),
        "a shortened smit was still sent"
    );
    let sent = &daemon.smits()[0].1;
    assert!(sent.chars().count() <= 48, "{sent}");
    // The target has no real repository, so the deploy attempt itself
    // fails - what this test pins is that the smit publish ahead of it
    // did not take the whole tick down too.
    assert!(results[0].1.is_err(), "the fetch is what fails here");
}

/// fails if `shep flock` keeps the old sha after a deploy until the next
/// tick. The first paint is from the record the tick read, so only a
/// paint after the deploy can carry the sha it landed.
#[tokio::test(start_paused = true)]
async fn a_deploy_repaints_the_smit_in_the_tick_that_made_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin = write_target_ready(home.path(), "fine", Watch::Auto);
    let head = fixtures::head_of(origin.path());
    let daemon = SmitRecording::default();

    let results = tick(&daemon, home.path(), &fixtures::dog_config())
        .await
        .results;

    assert!(
        matches!(results[0].1, Ok(Outcome::Deployed { .. })),
        "the deploy landed: {results:?}"
    );
    let last = daemon.smits().pop().expect("a smit was painted");
    assert_eq!(last, ("fine".to_owned(), format!("▲ main@{}", &head[..6])));
}

/// fails if a refused repaint is lost or takes the tick down. It is a
/// `{name}/smit` row like the first paint's, and the deploy's own row
/// stays.
#[tokio::test(start_paused = true)]
async fn a_refused_repaint_is_a_row_and_does_not_stop_the_tick() {
    let home = tempfile::tempdir().expect("tempdir");
    let _origin = write_target_ready(home.path(), "fine", Watch::Auto);

    let results = tick(
        &RefusingSmits(Ready::new()),
        home.path(),
        &fixtures::dog_config(),
    )
    .await
    .results;

    let smit_rows = results
        .iter()
        .filter(|(key, outcome)| key == "fine/smit" && outcome.is_err())
        .count();
    assert_eq!(
        smit_rows, 2,
        "first paint and repaint both refused: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|(key, outcome)| key == "fine" && matches!(outcome, Ok(Outcome::Deployed { .. }))),
        "the deploy still ran: {results:?}"
    );
}
