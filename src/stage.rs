//! `shep deploy <sheep> --stage`: the slow half of a deploy, on its own.
//!
//! A stage fetches, resolves the branch head, checks the release out and
//! builds it, then writes the sha into `deploy.toml` as `staged`. It never
//! swaps `current`, never reloads, and never touches `deployed` or `failed`,
//! which is why [`stage`] takes no [`crate::daemon::Daemon`] at all: a
//! function with no connection to the shepherd cannot start, reload or
//! restart anything even by accident.
//!
//! `shep deploy <sheep> --promote` is the other half: [`promote`] swaps to
//! what a stage built, reloads and verifies it, and takes the daemon a stage
//! never does.

use crate::build;
use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::{
    self, Outcome, head_of, read_staged, record, refuse_ungated_verification, run_release_hook,
    stage_release, swap_and_land,
};
use crate::error::Error;
use crate::paths::Tree;
use crate::settings::{self, Settings};
use crate::state::{State, Watch};
use crate::{git, linkcheck, lock, retention, shared, swap};

/// What one stage did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::enum_variant_names,
    reason = "`AlreadyStaged` names the answer, beside `AlreadyLive`"
)]
pub enum Staged {
    /// The head was built and recorded as staged.
    Built {
        /// The sha now staged.
        sha: String,
    },
    /// The head is already the staged sha and its release is built, so
    /// nothing was built.
    AlreadyStaged {
        /// The sha that is staged.
        sha: String,
    },
    /// The head is already deployed, so there is nothing to stage.
    AlreadyLive {
        /// The sha that is live.
        sha: String,
    },
}

/// Builds the head of `state.branch` into its release and records it as
/// `state.staged`, without swapping it in.
///
/// The build is `settings`' command, which the caller reads from the sheep's
/// stored config. Reclaims old releases afterwards, sparing `deployed` and `staged`, and
/// reports a failure to do so rather than returning it: the stage itself is
/// already decided.
///
/// `state` is replaced by the record as it is on disk once the tree is held.
///
/// # Errors
/// [`Error::AlreadyDeploying`] if another process holds the tree, and
/// [`Error::Io`] if the lock file cannot be created. [`Error::Io`] or
/// [`Error::Config`] if `deploy.toml` cannot be read or fails validation. [`Error::NotCutOver`] if
/// the record names no released sha, as for a deploy. [`Error::Config`] if
/// the target is `watch = auto`: the next poll tick would deploy the same
/// head, so a stage there promises something it cannot keep.
///
/// Then whatever the steps a deploy takes before its swap return: see
/// [`crate::git::fetch`], [`crate::deploy::head_of`] and
/// [`crate::build::run`]. A failed build records nothing, and is not held
/// against the sha: `failed` is the poll loop's and a stage is not its
/// attempt. [`Error::Io`] or [`Error::Config`] if the record cannot be
/// written, in which case `state.staged` is as it was.
pub async fn stage(
    tree: &Tree,
    state: &mut State,
    settings: &Settings,
    config: &DogConfig,
) -> Result<Staged, Error> {
    let sheep = tree.sheep();
    let _staging = lock::hold(tree)?;

    // The caller read `state` before the lock, and a deploy or another stage
    // may have written the record since. Answering from the older copy would
    // rebuild a head that is already staged or call a landed one new.
    *state = State::read(&tree.state_file())?;

    if state.deployed.is_none() {
        return Err(Error::NotCutOver {
            sheep: sheep.to_owned(),
            tree: tree.root().to_owned(),
        });
    }
    if state.watch == Watch::Auto {
        return Err(Error::Config(format!(
            "{sheep} is watch = auto, so the poll loop would deploy the head on its next tick \
             and a stage would not hold. Run `shep-deploy deploy {sheep} --watch manual` first."
        )));
    }

    let (git_dir, remote, tracked, budget) = (
        tree.git(),
        state.remote.clone(),
        state.branch.clone(),
        config.git_timeout,
    );
    shared::off_thread(move || git::fetch(&git_dir, &remote, &tracked, budget)).await?;
    let head = head_of(tree, &state.branch)?;
    if state.deployed.as_deref() == Some(head.as_str()) {
        return Ok(Staged::AlreadyLive { sha: head });
    }
    // Only while the release is still there to promote: `promote` names this
    // verb as the way back from a release that is gone or unfinished, and
    // answering "already staged" would leave no way to build it again.
    if state.staged.as_deref() == Some(head.as_str())
        && tree.release(&head).is_dir()
        && tree.completion(&head).is_file()
    {
        return Ok(Staged::AlreadyStaged { sha: head });
    }

    let built = build_and_record(tree, state, &head, settings, config).await;

    let spared: Vec<String> = [state.deployed.as_deref(), state.staged.as_deref()]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect();
    let pruned = {
        let (tree, keep) = (tree.clone(), config.retention);
        shared::off_thread(move || {
            let spared: Vec<&str> = spared.iter().map(String::as_str).collect();
            retention::prune(&tree, keep, &spared)
        })
        .await
    };
    if let Err(err) = pruned {
        eprintln!("shep-deploy: {sheep}: could not reclaim old releases: {err}");
    }

    built.map(|()| Staged::Built { sha: head })
}

/// Swaps `current` to the release [`stage`] built, reloads the sheep, and
/// verifies it, rolling back if it does not come up.
///
/// Runs no build and fetches nothing: the sha is the one `state.staged`
/// names, whatever the branch says now. The release hook runs first, since a
/// stage never ran it, and the rest is [`deploy::swap_and_land`] followed by
/// [`record`], so rollback, the held sha and retention are what a deploy's
/// are. A sha that failed is recorded in `failed` and left `staged`, so the
/// operator can promote it again. A hold on the sha does not stop a promote,
/// as it does not stop a deploy asked for by name.
///
/// `state` is replaced by the record as it is on disk once the tree is held.
///
/// # Errors
/// [`Error::AlreadyDeploying`] if another process holds the tree, and
/// [`Error::Io`] if the lock file cannot be created. [`Error::Io`] or
/// [`Error::Config`] if `deploy.toml` cannot be read or fails validation.
/// [`Error::NotCutOver`] if the record names no released sha, as for a
/// deploy. [`Error::Config`] if nothing is staged, or the staged release or
/// its completion marker is gone, naming `--stage` as the way back.
///
/// Then whatever the release hook and [`deploy::deploy`]'s steps from the
/// swap on return, with the same rollback endings.
pub async fn promote<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();
    let _promoting = lock::hold(tree)?;

    // The caller read `state` before the lock, and a deploy or a stage may
    // have written the record since.
    *state = State::read(&tree.state_file())?;

    if state.deployed.is_none() {
        return Err(Error::NotCutOver {
            sheep: sheep.to_owned(),
            tree: tree.root().to_owned(),
        });
    }
    let Some(head) = state.staged.clone() else {
        return Err(Error::Config(format!(
            "{sheep} has nothing staged. Run `shep-deploy deploy {sheep} --stage` first."
        )));
    };
    if !tree.release(&head).is_dir() || !tree.completion(&head).is_file() {
        return Err(Error::Config(format!(
            "{sheep} has {head} staged, but its release is gone or unfinished. Run \
             `shep-deploy deploy {sheep} --stage` to build it again."
        )));
    }

    let outcome = land_staged(daemon, tree, state, &head, config).await;
    record(tree, state, &head, &outcome, config.retention).await;
    outcome
}

/// The release hook, the swap and the verdict, for the staged `head`.
async fn land_staged<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    head: &str,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    let started_at = swap::resolve(&tree.current())?;
    let settings = settings::read(daemon, tree.sheep()).await?;
    let staged = {
        let (tree, checkout, head) = (tree.clone(), state.checkout.clone(), head.to_owned());
        shared::off_thread(move || read_staged(&tree, &checkout, &head, &settings)).await?
    };
    refuse_ungated_verification(tree.sheep(), &staged.app, state.verify)?;
    run_release_hook(tree, head, &staged, config).await?;
    swap_and_land(daemon, tree, state, head, started_at, &staged, config).await
}

/// The release, its build, and the record write, for one sha.
async fn build_and_record(
    tree: &Tree,
    state: &mut State,
    head: &str,
    settings: &Settings,
    config: &DogConfig,
) -> Result<(), Error> {
    let sheep = tree.sheep();
    let release = tree.release(head);
    let deploy::Staged {
        app, spec, linked, ..
    } = {
        let (tree, checkout, head) = (tree.clone(), state.checkout.clone(), head.to_owned());
        let settings = settings.clone();
        shared::off_thread(move || stage_release(&tree, &checkout, &head, &settings)).await?
    };
    refuse_ungated_verification(sheep, &app, state.verify)?;
    let snapshot = linkcheck::Snapshot::before(sheep, &release, &linked);
    let built = build::run(
        sheep,
        &release,
        &spec,
        app.user.as_deref(),
        &config.passthrough,
        &tree.cache_target(),
        config.build_timeout,
    )
    .await;
    for warning in snapshot.after() {
        eprintln!("{warning}");
    }
    built?;

    record_staged(tree, state, head)
}

/// Writes `head` as `staged` into the record on disk, under the record lock,
/// and makes `state` that record.
///
/// Built from the disk, not from `state`: the build took as long as it took,
/// and `--branch`, `--watch` or a hand edit may have written the record
/// since. Writing `state` whole put back a `failed` that `--branch` had just
/// cleared, beside a `staged` from the branch the target no longer follows.
///
/// # Errors
/// [`Error::Config`] if the target's branch is not the one `head` was built
/// from: nothing is written, and `state` becomes the record as it is, so the
/// operator's move stands and the next stage builds the new branch's head.
/// [`Error::Io`] if the record lock cannot be taken, and whatever
/// [`State::read`] and [`State::write`] return, in which case `state` is as
/// it was.
fn record_staged(tree: &Tree, state: &mut State, head: &str) -> Result<(), Error> {
    let _record = lock::hold_record(tree)?;
    let on_disk = State::read(&tree.state_file())?;
    if on_disk.branch != state.branch {
        let message = format!(
            "{}: the branch changed from {} to {} while {head} was building, so it was not \
             staged. Run the stage again for the new branch.",
            tree.sheep(),
            state.branch,
            on_disk.branch
        );
        *state = on_disk;
        return Err(Error::Config(message));
    }
    let next = State {
        staged: Some(head.to_owned()),
        ..on_disk
    };
    next.write(&tree.state_file())?;
    *state = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::deploy::Outcome;
    use crate::deploy::test_support::{
        Shepherd, commit_on_origin, fixture_before_any_release, fixture_with_previous_release,
    };
    use crate::{fixtures, swap};

    use super::*;

    /// The sheep's settings with a build that runs `command`.
    fn building(command: &str) -> Settings {
        Settings {
            command: Some(command.to_owned()),
            ..Settings::default()
        }
    }

    /// A build that leaves `built.marker` in the release.
    fn touching_marker() -> Settings {
        building("touch built.marker")
    }

    /// Stages the head of a new commit whose build runs `command`, returning
    /// its sha. The record is on disk and in `fixture.state`.
    async fn stage_a_build(
        fixture: &mut crate::deploy::test_support::Fixture,
        command: &str,
    ) -> String {
        let second = commit_on_origin(fixture, "second.txt");
        stage(
            &fixture.tree,
            &mut fixture.state,
            &building(command),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages");
        second
    }

    /// fails if a promote builds again, does not reload, or leaves `staged`
    /// behind. The marker is removed after the stage, so a second build would
    /// put it back; the reload count is what separates a swap from a
    /// symlink move.
    #[tokio::test]
    async fn promote_swaps_to_the_staged_release_and_clears_the_record() {
        let mut fixture = fixture_with_previous_release();
        let second = stage_a_build(&mut fixture, "touch built.marker").await;
        let marker = fixture.tree.release(&second).join("built.marker");
        fs::remove_file(&marker).expect("marker");
        let daemon = Shepherd::ready();

        let outcome = promote(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("promotes");

        assert_eq!(
            outcome,
            Outcome::Deployed {
                sha: second.clone()
            }
        );
        assert!(!marker.exists(), "no build ran");
        assert_eq!(daemon.reload_count(), 1);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            Some(fixture.tree.release(&second))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(second.as_str()));
        assert_eq!(fixture.state.staged, None);
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.deployed.as_deref(), Some(second.as_str()));
        assert_eq!(written.staged, None);
        assert_eq!(written.failed, None);
    }

    /// fails if a promote with nothing staged builds, swaps or reloads
    /// anything, or changes the record. The branch has a newer head, so
    /// deploying "the head" instead of refusing is the symptom.
    #[tokio::test]
    async fn promote_with_nothing_staged_is_refused_and_touches_nothing() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let before = std::fs::read_to_string(fixture.tree.state_file()).expect("deploy.toml");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();

        let err = promote(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("refuses");

        assert!(matches!(err, Error::Config(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("--stage"), "{shown}");
        assert_eq!(daemon.reload_count(), 0);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(
            fs::read_to_string(fixture.tree.state_file()).expect("deploy.toml"),
            before
        );
    }

    /// fails if a promote lands the branch's newest head rather than the sha
    /// that was staged. The operator approved what was built, and the
    /// branch moving since is a reason to stage again, not to deploy
    /// something nobody built.
    #[tokio::test]
    async fn promote_after_the_branch_moved_still_deploys_the_staged_sha() {
        let mut fixture = fixture_with_previous_release();
        let second = stage_a_build(&mut fixture, "true").await;
        let third = commit_on_origin(&fixture, "third.txt");
        let remote = fixture.state.remote.clone();
        git::fetch(&fixture.tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");
        assert_eq!(
            git::remote_head(&fixture.tree.git(), "main").expect("head"),
            third,
            "the branch has moved on"
        );

        let outcome = promote(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("promotes");

        assert_eq!(
            outcome,
            Outcome::Deployed {
                sha: second.clone()
            }
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(second.as_str()));
        assert!(
            !fixture.tree.release(&third).exists(),
            "the head was not built"
        );
    }

    /// fails if a promote swaps to a release that retention or a hand has
    /// removed, or one whose completion marker is gone, or does not say how
    /// to get a release back. Either would point `current` at a directory
    /// that is missing or half there.
    #[tokio::test]
    async fn promote_with_the_staged_release_gone_names_stage() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = stage_a_build(&mut fixture, "true").await;
        let daemon = Shepherd::ready();

        fs::remove_file(fixture.tree.completion(&second)).expect("marker");
        let err = promote(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no marker");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("--stage"), "{err}");

        fs::remove_dir_all(fixture.tree.release(&second)).expect("release");
        let err = promote(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no release");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("--stage"), "{err}");

        assert_eq!(daemon.reload_count(), 0);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.staged.as_deref(), Some(second.as_str()));
    }

    /// fails if a promote reads configuration the build rewrote. The build
    /// runs as the app's user and owns the release, so a Flockfile it
    /// changed after the stage is not the one the commit carries. Neither
    /// the app nor the hooks may come from it.
    #[tokio::test]
    async fn promote_refuses_a_flockfile_the_build_changed() {
        for command in [
            "echo '# changed' >> Flockfile.toml",
            "echo '' > Flockfile.override.toml",
        ] {
            let mut fixture = fixture_with_previous_release();
            let previous = fixture.state.deployed.clone().expect("a previous release");
            stage_a_build(&mut fixture, command).await;
            let daemon = Shepherd::ready();

            let err = promote(
                &daemon,
                &fixture.tree,
                &mut fixture.state,
                &fixtures::dog_config(),
            )
            .await
            .expect_err("refuses");

            assert!(matches!(err, Error::Config(_)), "{command}: {err:?}");
            assert_eq!(daemon.reload_count(), 0, "{command}");
            assert_eq!(
                swap::resolve(&fixture.tree.current()).expect("reads"),
                Some(fixture.tree.release(&previous)),
                "{command}"
            );
        }
    }

    /// fails if a promote that does not come up leaves the new release live,
    /// or forgets what was staged. Keeping `staged` is what lets the operator
    /// promote again once the cause is fixed, without paying for the build.
    #[tokio::test(start_paused = true)]
    async fn a_promote_that_does_not_come_up_is_rolled_back_and_stays_staged() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        // No build command: the paused clock would time a real one out.
        let second = commit_on_origin(&fixture, "second.txt");
        stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages");

        let outcome = promote(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
        assert_eq!(fixture.state.staged.as_deref(), Some(second.as_str()));
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.deployed.as_deref(), Some(previous.as_str()));
        assert_eq!(written.staged.as_deref(), Some(second.as_str()));
        assert!(
            fixture.tree.release(&second).exists(),
            "retention spares what is staged"
        );
    }

    /// fails if a stage swaps, or fails to leave a built release and a
    /// record behind. This is the whole feature: the build is paid for now,
    /// and `current`, `deployed` and `failed` read as they did before.
    #[tokio::test]
    async fn staging_builds_the_head_and_records_it_without_moving_current() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        let staged = stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages");

        assert_eq!(
            staged,
            Staged::Built {
                sha: second.clone()
            }
        );
        assert!(
            fixture.tree.release(&second).join("built.marker").exists(),
            "the build ran in the release"
        );
        assert_eq!(fixture.state.staged.as_deref(), Some(second.as_str()));
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.staged.as_deref(), Some(second.as_str()));
        assert_eq!(written.deployed.as_deref(), Some(previous.as_str()));
        assert_eq!(written.failed, None);
        assert_eq!(
            swap::resolve(&fixture.tree.current()).expect("reads"),
            Some(fixture.tree.release(&previous))
        );
    }

    /// fails if the deployed head is built again under the name of a stage.
    /// There is nothing to stage, and saying so is the answer.
    #[tokio::test]
    async fn staging_the_deployed_head_says_it_is_already_live() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");

        let staged = stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect("answers");

        assert_eq!(staged, Staged::AlreadyLive { sha: live });
        assert_eq!(fixture.state.staged, None);
    }

    /// fails if asking twice builds twice. The marker is removed between the
    /// two, so a second build would put it back.
    #[tokio::test]
    async fn staging_twice_is_a_no_op_the_second_time() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages");
        let marker = fixture.tree.release(&second).join("built.marker");
        fs::remove_file(&marker).expect("marker");

        let again = stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("answers");

        assert_eq!(again, Staged::AlreadyStaged { sha: second });
        assert!(!marker.exists(), "nothing was built");
    }

    /// fails if staging cannot rebuild a staged release that is gone or
    /// unfinished. Promote names `--stage` as the way out of both, so an
    /// "already staged" answer while the head has not moved leaves the
    /// operator with no way to get a release back.
    #[tokio::test]
    async fn staging_again_rebuilds_a_staged_release_that_is_gone_or_unfinished() {
        let mut fixture = fixture_with_previous_release();
        let second = stage_a_build(&mut fixture, "touch built.marker").await;
        let release = fixture.tree.release(&second);

        fs::remove_file(fixture.tree.completion(&second)).expect("marker");
        let again = stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages again");
        assert_eq!(
            again,
            Staged::Built {
                sha: second.clone()
            }
        );
        assert!(fixture.tree.completion(&second).is_file());

        fs::remove_dir_all(&release).expect("release");
        let again = stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages again");
        assert_eq!(
            again,
            Staged::Built {
                sha: second.clone()
            }
        );
        assert!(release.join("built.marker").exists(), "it was built again");
        assert_eq!(fixture.state.staged.as_deref(), Some(second.as_str()));
    }

    /// fails if a newer head leaves the older sha as the staged one. The
    /// operator would then land a release the branch has moved past.
    #[tokio::test]
    async fn staging_a_newer_head_replaces_the_staged_sha() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");
        stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages");
        let third = commit_on_origin(&fixture, "third.txt");

        let staged = stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect("stages again");

        assert_eq!(staged, Staged::Built { sha: third.clone() });
        assert_eq!(fixture.state.staged.as_deref(), Some(third.as_str()));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml")
                .staged
                .as_deref(),
            Some(third.as_str())
        );
    }

    /// fails if a build that exits non-zero leaves a staged sha, or marks
    /// the sha failed. `failed` is the poll loop's hold on a sha it
    /// attempted, and a stage is not an attempt: holding it would make the
    /// next `deploy` by the poll loop skip a head nothing ever tried to land.
    #[tokio::test]
    async fn a_failing_build_records_nothing_and_does_not_hold_the_sha() {
        let mut fixture = fixture_with_previous_release();
        commit_on_origin(&fixture, "second.txt");

        let err = stage(
            &fixture.tree,
            &mut fixture.state,
            &building("exit 3"),
            &fixtures::dog_config(),
        )
        .await
        .expect_err("the build fails");

        assert!(matches!(err, Error::Build { status: Some(3) }), "{err:?}");
        assert_eq!(fixture.state.staged, None);
        assert_eq!(fixture.state.failed, None);
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.staged, None);
        assert_eq!(written.failed, None);
    }

    /// fails if a `watch = auto` target is staged, or is fetched before it
    /// is refused. The poll loop would deploy the same head on its next tick,
    /// so the stage would promise what it cannot keep.
    #[tokio::test]
    async fn a_watched_target_is_refused_and_nothing_is_fetched() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        fixture.state.watch = Watch::Auto;
        fixture
            .state
            .write(&fixture.tree.state_file())
            .expect("write deploy.toml");
        commit_on_origin(&fixture, "second.txt");

        let err = stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect_err("refuses");

        assert!(matches!(err, Error::Config(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("--watch manual"), "{shown}");
        assert_eq!(fixture.state.staged, None);
        assert_eq!(
            git::remote_head(&fixture.tree.git(), "main").expect("head"),
            previous,
            "the clone still has the head it had before"
        );
    }

    /// fails if a tree the cutover never landed on is staged. A stage there
    /// builds a release for a sheep that has never served from the tree.
    #[tokio::test]
    async fn a_tree_the_cutover_never_landed_on_is_refused() {
        let mut fixture = fixture_before_any_release();

        let err = stage(
            &fixture.tree,
            &mut fixture.state,
            &Settings::default(),
            &fixtures::dog_config(),
        )
        .await
        .expect_err("refuses");

        assert!(matches!(err, Error::NotCutOver { .. }), "{err:?}");
        assert_eq!(fixture.state.staged, None);
    }

    /// fails if a stage answers from the record its caller read before the
    /// lock. A deploy or another stage may have written it in between; here
    /// the disk already names the head as staged, and the stale copy does
    /// not, so building again is the symptom.
    #[tokio::test]
    async fn a_stage_answers_from_the_record_on_disk_not_the_callers_copy() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        State {
            staged: Some(second.clone()),
            ..fixture.state.clone()
        }
        .write(&fixture.tree.state_file())
        .expect("another stage's record");
        // What that stage left: a finished release, unbuilt here.
        fs::create_dir_all(fixture.tree.release(&second)).expect("release");
        fs::create_dir_all(fixture.tree.completions()).expect("markers");
        fs::write(fixture.tree.completion(&second), b"").expect("marker");

        let staged = stage(
            &fixture.tree,
            &mut fixture.state,
            &touching_marker(),
            &fixtures::dog_config(),
        )
        .await
        .expect("answers");

        assert_eq!(
            staged,
            Staged::AlreadyStaged {
                sha: second.clone()
            }
        );
        assert!(
            !fixture.tree.release(&second).join("built.marker").exists(),
            "nothing was built"
        );
        assert_eq!(fixture.state.staged.as_deref(), Some(second.as_str()));
    }

    /// fails if a stage's record write puts back what `--branch` changed
    /// while it built. The stale copy still holds the old branch and the
    /// `failed` that `--branch` clears; writing it whole recorded a `staged`
    /// from the branch the target no longer follows.
    #[test]
    fn a_branch_set_while_a_stage_builds_is_not_overwritten() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.failed = Some(fixtures::OTHER_SHA.to_owned());
        fixture
            .state
            .write(&fixture.tree.state_file())
            .expect("write deploy.toml");
        let mut in_flight = fixture.state.clone();

        crate::retarget::set_branch(&fixture.tree, "release").expect("--branch release");
        let err = record_staged(&fixture.tree, &mut in_flight, fixtures::SHA)
            .expect_err("the branch moved");

        assert!(matches!(err, Error::Config(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.contains("main to release"), "{shown}");
        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.branch, "release");
        assert_eq!(written.failed, None);
        assert_eq!(written.staged, None);
        assert_eq!(in_flight, written, "memory follows the disk");
    }

    /// fails if a stage records its own stale copy of `failed`. A hold put
    /// on the record while the build ran is not the stage's to drop.
    #[test]
    fn a_stage_leaves_the_record_fields_it_does_not_own() {
        let fixture = fixture_with_previous_release();
        let mut in_flight = fixture.state.clone();
        State {
            failed: Some(fixtures::OTHER_SHA.to_owned()),
            ..fixture.state.clone()
        }
        .write(&fixture.tree.state_file())
        .expect("a hold written meanwhile");

        record_staged(&fixture.tree, &mut in_flight, fixtures::SHA).expect("records");

        let written = State::read(&fixture.tree.state_file()).expect("deploy.toml");
        assert_eq!(written.staged.as_deref(), Some(fixtures::SHA));
        assert_eq!(written.failed.as_deref(), Some(fixtures::OTHER_SHA));
        assert_eq!(in_flight, written);
    }
}
