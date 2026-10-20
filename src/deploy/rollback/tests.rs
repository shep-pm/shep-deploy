use std::fs;

use crate::deploy::test_support::{
    FLOCKFILE, RECLAIMED, Shepherd, commit_on_origin, fixture_before_any_release,
    fixture_with_previous_release, install_release,
};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::paths::Tree;
use crate::state::State;
use crate::{fixtures, swap};

/// fails if a rollback the shepherd never accepts is reported as
/// anything vaguer than the state it leaves. This is the one situation
/// this crate cannot repair on its own, and the operator gets three
/// things to compare and one command to run: `current` and deploy.toml
/// agree on the old release, and the process may still be on the new
/// one.
#[tokio::test(start_paused = true)]
async fn a_rollback_the_shepherd_never_accepts_is_reported_as_a_split_state() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    let second = commit_on_origin(&fixture, "second.txt");

    let err = deploy(
        &Shepherd::busy_for(u32::MAX),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the rollback cannot reload");

    // Not wrapped in `RollbackFailed`: that wrapper's tail tells an
    // operator to go and compare the three things `Split` has just
    // finished telling them.
    assert!(matches!(err, Error::Split { .. }), "{err:?}");
    let shown = err.to_string();
    assert!(!shown.contains("rolling back after"), "{shown}");
    assert!(
        shown.contains("no reload onto it could be confirmed"),
        "{shown}"
    );
    assert!(shown.contains(&previous), "{shown}");
    assert!(shown.contains(&second), "{shown}");
    assert!(shown.contains("shep reload web"), "{shown}");

    // The two things this crate does control agree with each other, so
    // the message has exactly one disagreement to explain.
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    assert_eq!(
        State::read(&fixture.tree.state_file())
            .expect("deploy.toml was written")
            .deployed
            .as_deref(),
        Some(previous.as_str())
    );
}

/// fails if a failure between the swap and the reload escapes the
/// rollback boundary. The generation capture is a real request and can
/// fail like any other; when it did, `deploy` returned the bare error
/// with `current` still naming the new release, `deploy.toml` naming
/// the old one, and no reload ever sent. Any later restart of that
/// sheep - a crash restart included - would have brought it up on a
/// release nothing verified. It was the only fallible call past the
/// swap that sat outside the arm that owns rollback.
#[tokio::test(start_paused = true)]
async fn a_failure_before_the_reload_puts_current_back() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::unreachable();

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the shepherd cannot be asked anything");

    assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    assert_eq!(daemon.attempt_count(), 0, "no reload may be sent");
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous)),
        "current must not be left on a release nothing verified"
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
}

/// fails if a `current` already naming the attempted release, with no
/// earlier link to put back, is left there after a failure before the
/// reload. That is the shape a deploy killed between its swap and its
/// record write leaves, and `undo_swap` used to return `Ok` for it with
/// `current` on a release nothing verified. The record still names the
/// good release, and that is what goes back.
#[tokio::test(start_paused = true)]
async fn a_failure_before_the_reload_puts_current_back_from_the_record() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    let attempted = commit_on_origin(&fixture, "second.txt");
    // As a killed deploy leaves it: `current` already on the attempted
    // release, the record still on the previous one.
    swap::point_at(&fixture.tree.current(), &fixture.tree.release(&attempted))
        .expect("point current at the attempted release");

    let err = deploy(
        &Shepherd::unreachable(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the shepherd cannot be asked anything");

    assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous)),
        "current goes back to the release the record names"
    );
}

/// fails if a `current` an operator repointed at something that is not a
/// release is taken as the rollback target. `previous` is the raw text
/// of the link, and a rollback onto `<tree>/anything` wrote
/// `deployed = "anything"` into the record, which `State::read` then
/// refused on every later command of every kind. The record's own release
/// is what a rollback returns to instead.
#[tokio::test(start_paused = true)]
async fn a_current_that_names_no_release_is_not_a_rollback_target() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let elsewhere = fixture.tree.root().join("not-a-release");
    fs::create_dir_all(&elsewhere).expect("a directory that is not a release");
    swap::point_at(&fixture.tree.current(), &elsewhere).expect("repoint current by hand");

    let outcome = deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("rolled back onto the recorded release");

    assert!(
        matches!(&outcome, Outcome::RolledBack { to, .. } if *to == previous),
        "{outcome:?}"
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
    State::read(&fixture.tree.state_file()).expect("the record is still readable");
}

/// fails if a rollback whose own reload is never confirmed stops
/// reporting the split state. This is the arm that had no test at any
/// tier until the fake learned to lose a reply: `restore` swaps back,
/// corrects the record, and then cannot get an answer about the reload
/// for the whole budget, so which release the instances are on is
/// genuinely unknown.
#[tokio::test(start_paused = true)]
async fn a_rollback_that_is_never_confirmed_reports_the_split_state() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");

    let err = deploy(
        &Shepherd::losing_replies(u32::MAX),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("no reload is ever confirmed");

    assert!(matches!(err, Error::Split { .. }), "{err:?}");
    let shown = err.to_string();
    assert!(shown.contains("could be confirmed"), "{shown}");
    assert!(shown.contains(&previous), "{shown}");
    // The two things this crate controls still agree with each other.
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
}

/// fails if a rollback moves the symlink and never reloads. Moving a
/// symlink does not move a running process: without the second reload
/// the daemon keeps running the release that was just rejected, while
/// `current` and `deploy.toml` both name the old one, and an operator
/// is told the rollback succeeded.
#[tokio::test(start_paused = true)]
async fn a_rollback_reloads_onto_the_previous_release() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::never_ready();

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert!(matches!(outcome, Outcome::RolledBack { .. }));
    assert_eq!(
        daemon.reload_count(),
        2,
        "one reload onto the new release, one onto the old"
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
}

/// fails if a rollback decides "there is nothing to go back to" from
/// `current` alone. A deploy killed between its swap and its
/// `State::write` is exactly the interruption `restore`'s record
/// correction exists to repair, and it leaves `current` already naming
/// the release being attempted while `deploy.toml` still names a good
/// one whose worktree is on disk.
///
/// Read `current` alone and the retry concludes `previous == attempted`,
/// reports "there is nothing to roll back to", attempts no rollback
/// reload, and leaves a release that will not come up serving.
#[tokio::test(start_paused = true)]
async fn a_rollback_reads_deploy_toml_when_current_is_already_the_new_release() {
    let mut fixture = fixture_with_previous_release();
    let live = fixture.state.deployed.clone().expect("a previous release");
    let second = commit_on_origin(&fixture, "second.txt");

    // What an interrupted deploy leaves behind: the swap happened, the
    // record write did not.
    crate::git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    install_release(&fixture, &second);
    assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));

    let daemon = Shepherd::never_ready();
    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::RolledBack {
            to: live.clone(),
            why: "it did not come up and stay up, 16s after the reload".to_owned(),
        }
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&live))
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));
    assert_eq!(
        State::read(&fixture.tree.state_file())
            .expect("deploy.toml")
            .deployed
            .as_deref(),
        Some(live.as_str())
    );
    // The deploy's own reload, and the rollback's onto the recorded
    // release. The second is the one the old code never sent.
    assert_eq!(daemon.attempt_count(), 2);
}

/// fails if the release `current` names is trusted to exist. The
/// existence guard was added for the record's branch and not for this
/// one, which left the first source able to do the exact harm the guard
/// was added for: `current` naming a release that retention has swept
/// is a dangling link, and rolling back "onto" it repoints `current` at
/// a path that is not there.
///
/// The sheep's `cwd` is `current`, permanently, so that is not a
/// cosmetic failure - it is an app that cannot start at all, which is
/// worse than the release that would not come up.
#[tokio::test(start_paused = true)]
async fn a_current_naming_a_swept_release_is_not_a_rollback_target() {
    let mut fixture = fixture_with_previous_release();
    let live = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");

    // Retention swept the release `current` still points at, so the
    // link is there and its target is not.
    crate::git::worktree_remove(&fixture.tree.git(), &fixture.tree.release(&live))
        .expect("retention removes the worktree");
    assert!(swap::resolve(&fixture.tree.current()).unwrap().is_some());
    assert!(!fixture.tree.release(&live).exists());

    let err = deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("nothing usable to roll back to");

    assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
    assert!(
        swap::resolve(&fixture.tree.current())
            .unwrap()
            .is_some_and(|path| path.exists()),
        "current must never be repointed at a release that is not there"
    );
}

/// fails if a sha in `deploy.toml` is treated as a release that is
/// there. Retention removes old worktrees, so the record can name one
/// that no longer exists on disk, and rolling back onto it would point
/// `current` at a dangling link - a sheep whose `cwd` resolves to
/// nothing, which is worse than the release that would not come up.
#[tokio::test(start_paused = true)]
async fn a_recorded_release_that_is_gone_is_not_a_rollback_target() {
    let mut fixture = fixture_with_previous_release();
    let live = fixture.state.deployed.clone().expect("a previous release");
    let second = commit_on_origin(&fixture, "second.txt");

    // The same interrupted deploy as above, except that retention has
    // since swept the release the record names.
    crate::git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    install_release(&fixture, &second);
    crate::git::worktree_remove(&fixture.tree.git(), &fixture.tree.release(&live))
        .expect("retention removes the old worktree");
    assert!(!fixture.tree.release(&live).exists());

    let err = deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("nothing left to roll back to");

    assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&second)),
        "current stays where it was rather than dangling"
    );
}

/// fails if a deploy that never comes up with nothing to fall back to
/// is reported as a rollback that failed. Nothing was rolled back,
/// because there was nothing to roll back to, and the wrapped form said
/// "no previous release" and "still pointed at" once each in both of
/// its layers.
///
/// The record names a release retention has already reclaimed, which is
/// how a real target reaches this. It used to name nothing at all, and
/// that shape is now refused before the deploy starts: a tree whose
/// cutover never landed has no business being deployed.
#[tokio::test(start_paused = true)]
async fn a_deploy_with_nothing_left_to_fall_back_to_says_so_plainly() {
    let mut fixture = fixture_before_any_release();
    fixture.state.deployed = Some(RECLAIMED.to_owned());
    let first = crate::git::remote_head(&fixture.tree.git(), "main").expect("head");

    let err = deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("nothing to roll back to");

    assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
    let shown = err.to_string();
    assert!(shown.contains("web"), "{shown}");
    assert!(shown.contains(&first), "{shown}");
    // Not the number, which follows the app: what matters here is that
    // the reason survives into the sentence at all.
    assert!(shown.contains("did not come up and stay up"), "{shown}");
    assert!(shown.contains("after the reload"), "{shown}");
    assert!(!shown.contains("rolling back after"), "{shown}");
    assert_eq!(shown.matches("roll back").count(), 1, "{shown}");
    // It says what it looked at rather than asserting which case this
    // is. "This is its first deploy" is true here and false on a retry
    // after an interrupted one, which reaches the same variant.
    assert!(shown.contains("current"), "{shown}");
    assert!(shown.contains("deploy.toml"), "{shown}");
    assert!(!shown.contains("first deploy"), "{shown}");
    // The record never advanced, because nothing verified.
    assert_eq!(fixture.state.deployed.as_deref(), Some(RECLAIMED));
}

/// fails if "is there anything to roll back to" is decided by comparing
/// path text. `previous` is link text read off disk and `attempted` is
/// a sha this process just resolved, and one directory can be spelled
/// two ways: `$SHEP_HOME` reaches this crate through
/// `std::path::absolute`, which does not resolve symlinks, and the
/// supervised dog and the operator's own CLI can each arrive at a
/// different literal path. With a `deploy.toml` that lags `current`,
/// which is the very case `restore` exists to repair, a path
/// comparison then reports rolling back to the release that just
/// failed - the same one, under its other name.
#[tokio::test(start_paused = true)]
async fn a_previous_release_under_another_path_is_not_a_rollback_target() {
    let mut fixture = fixture_with_previous_release();
    let second = commit_on_origin(&fixture, "second.txt");
    crate::git::fetch(
        &fixture.tree.git(),
        &fixture.state.remote,
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    install_release(&fixture, &second);
    // What an interrupted deploy leaves: `current` is on `second` and
    // the record still names an older release, one retention has since
    // reclaimed, so `current` is the only candidate left.
    fixture.state.deployed = Some(RECLAIMED.to_owned());

    // The same directory, spelled through a symlink - one process's
    // $SHEP_HOME, another process's.
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let link = elsewhere.path().join("home");
    std::os::unix::fs::symlink(fixture.home.path(), &link).expect("symlink the home");
    let same_tree = Tree::for_sheep(&link, "web");

    let err = deploy(
        &Shepherd::never_ready(),
        &same_tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("there is no other release to go back to");

    assert!(matches!(err, Error::Unverified { .. }), "{err:?}");
}

/// fails if a rollback repairs the filesystem and leaves the record
/// wrong. A deploy killed between its reload and its `State::write`
/// leaves `deploy.toml` lagging `current`; the next rollback is the
/// moment that can be noticed, and a rollback that never writes leaves
/// the record wrong indefinitely.
#[tokio::test(start_paused = true)]
async fn a_rollback_corrects_a_stale_deployed_record() {
    let mut fixture = fixture_with_previous_release();
    let live = fixture.state.deployed.clone().expect("a previous release");
    // What an interrupted deploy leaves behind: `current` names the
    // live release and the record still names an older one, since
    // reclaimed.
    fixture.state.deployed = Some(RECLAIMED.to_owned());
    commit_on_origin(&fixture, "second.txt");

    deploy(
        &Shepherd::never_ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(fixture.state.deployed.as_deref(), Some(live.as_str()));
    assert_eq!(
        State::read(&fixture.tree.state_file())
            .expect("deploy.toml was written")
            .deployed
            .as_deref(),
        Some(live.as_str())
    );
}

/// fails if a rollback onto a release that spawns and never becomes
/// ready is reported as a rollback. Exit 12 promises a healthy flock on
/// the old release, and the reload being accepted says nothing of the
/// sort. `current` and the record still name the old release: the
/// filesystem is right, and what is running is not known.
#[tokio::test(start_paused = true)]
async fn a_rollback_onto_a_release_that_never_comes_up_is_reported_unverified() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::rollback_never_ready();

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("neither release comes up");

    let Error::RollbackUnverified {
        sheep,
        to,
        why,
        found,
    } = &err
    else {
        panic!("expected an unverified rollback: {err:?}");
    };
    assert_eq!(sheep, "web");
    assert_eq!(to, &previous);
    assert!(why.contains("did not come up and stay up"), "{why}");
    assert!(found.contains("did not come up and stay up"), "{found}");
    assert_eq!(daemon.reload_count(), 2);
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
    assert_eq!(fixture.state.deployed.as_deref(), Some(previous.as_str()));
    assert_eq!(
        State::read(&fixture.tree.state_file())
            .expect("deploy.toml")
            .deployed
            .as_deref(),
        Some(previous.as_str())
    );
}

/// fails if a sheep the shepherd dropped is reported as rolled back.
/// This is what shep does when it cannot spawn the release it was asked
/// to go back to: the reload is accepted, and `shep flock` then has no
/// row for the sheep. It is not a swap or a record write that failed,
/// so it is not `RollbackFailed`, and the shepherd's own words are what
/// an operator needs.
#[tokio::test(start_paused = true)]
async fn a_rollback_onto_a_sheep_the_shepherd_dropped_is_reported_unverified() {
    let mut fixture = fixture_with_previous_release();
    commit_on_origin(&fixture, "second.txt");

    let err = deploy(
        &Shepherd::rollback_drops_the_sheep(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the sheep is gone");

    assert!(!matches!(err, Error::RollbackFailed { .. }), "{err:?}");
    let Error::RollbackUnverified { found, .. } = &err else {
        panic!("expected an unverified rollback: {err:?}");
    };
    assert!(found.contains("no sheep named web"), "{found}");
}

/// fails if a rollback that does come up stops being reported as one.
/// The check added to `restore` must not turn the ordinary, healthy
/// rollback into an error: exit 12 still means a verified rollback.
#[tokio::test(start_paused = true)]
async fn a_rollback_that_comes_up_is_still_reported_rolled_back() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::never_ready();

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("a healthy rollback is an outcome");

    assert!(
        matches!(&outcome, Outcome::RolledBack { to, .. } if *to == previous),
        "{outcome:?}"
    );
    assert_eq!(daemon.reload_count(), 2);
}

/// fails if `Alive` stops asking the rollback for a turnover. A probeless
/// app is judged by its pid changing and holding, and a shepherd that
/// keeps the old instance through both reloads has shown the rollback
/// nothing, so it is unverified rather than rolled back.
#[tokio::test(start_paused = true)]
async fn an_alive_rollback_that_never_turns_over_is_reported_unverified() {
    let mut fixture = fixture_with_previous_release();
    fixture.state.verify = crate::state::Verify::Alive;
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::never_turns_over();

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the rollback never turns the flock over");

    assert!(matches!(err, Error::RollbackUnverified { .. }), "{err:?}");
    assert_eq!(daemon.reload_count(), 2);
}

/// fails if the rollback is timed by the Flockfile of the release it is
/// rolling back FROM. The rejected commit here shortens its own timeouts,
/// so its budget is a few seconds; the release being restored has the
/// ordinary ones and takes longer than that to come up. Judged by the
/// rejected release's budget, a healthy rollback is reported as one that
/// never came up, and exit 1 says nothing is serving when the old
/// release is.
#[tokio::test(start_paused = true)]
async fn the_rollback_is_timed_by_the_release_being_restored() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    let impatient = FLOCKFILE.replace(
        "script = './run.sh'\n",
        "script = './run.sh'\nlisten_timeout = '1ms'\ngraceful_timeout = '1ms'\n",
    );
    assert_ne!(impatient, FLOCKFILE);
    fs::write(fixture.origin.path().join("Flockfile.toml"), impatient).expect("write");
    fixtures::run_git(fixture.origin.path(), &["add", "."]);
    fixtures::run_git(fixture.origin.path(), &["commit", "-q", "-m", "impatient"]);
    // Eight seconds of looks: past the rejected release's budget, well
    // inside the restored one's.
    let daemon = Shepherd::never_ready().rollback_ready_after(80);

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("the restored release comes up inside its own budget");

    assert!(
        matches!(&outcome, Outcome::RolledBack { to, .. } if *to == previous),
        "{outcome:?}"
    );
}

/// fails if a rollback that cannot swap back reports only half of what
/// happened. Either half alone is undiagnosable: the original failure
/// says what the machine was trying to do, and the rollback's own
/// failure says what state it is in now - `current` still naming a
/// release that never came up.
#[tokio::test(start_paused = true)]
async fn a_swap_back_that_fails_reports_both_failures() {
    let mut fixture = fixture_with_previous_release();
    commit_on_origin(&fixture, "second.txt");
    let stale = fixture.tree.current().with_file_name("current.tmp");

    let err = deploy(
        &Shepherd::planting(stale),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("the swap back collides with the file in the way");

    assert!(matches!(err, Error::RollbackFailed { .. }), "{err:?}");
    let shown = err.to_string();
    assert!(shown.contains("did not come up"), "{shown}");
    assert!(shown.contains("current.tmp"), "{shown}");
}
