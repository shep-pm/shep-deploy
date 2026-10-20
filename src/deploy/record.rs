//! What a deploy writes into `deploy.toml`, and the one setting an operator
//! can change without deploying.

use crate::deploy::Outcome;
use crate::error::Error;
use crate::paths::Tree;
use crate::state::{State, Watch};
use crate::{lock, retention, shared};

/// Writes down what this attempt on `head` came to, and reclaims old
/// releases.
///
/// Two pieces of bookkeeping that belong to every ending rather than to
/// one of them, and both used to sit in the verified arm alone.
///
/// `failed` is what stops the poll loop attempting one bad commit forever.
/// `deployed` advances only on a verify, so a sha that did not land leaves
/// the head and the record different, and the loop would find that
/// difference again on every tick. Recorded rather than remembered, so it
/// survives the dog being restarted, which is when a supervisor would
/// otherwise turn a held sha back into a rebuild loop.
///
/// Retention runs on every ending for the same shape of reason: the target
/// that keeps failing is the one building a fresh worktree for every commit
/// somebody pushes, and pruning only after a verify left exactly that one
/// growing without limit. [`retention::prune`] spares whatever `current`
/// names, so this is the same call the verified path always made.
///
/// Neither failure is fatal and neither is silent. The record write is
/// reported and passed over because the deploy's own outcome is already
/// decided and true - what a lost write costs is one more attempt at the
/// same sha - and the prune failure has always been reported this way; see
/// [`crate::retention`]'s own doc.
pub(super) async fn record(
    tree: &Tree,
    state: &mut State,
    head: &str,
    outcome: &Result<Outcome, Error>,
    keep: usize,
) {
    let sheep = tree.sheep();
    let landed = matches!(outcome, Ok(Outcome::Deployed { .. }));
    // `Raced` is held like any other ending. It was nearly exempted on
    // 2026-09-04 as "transient", and it is not: the tree lock means no rival
    // deploy can move `current` under this one any more, so what is left
    // moving it is an operator's hand or the release's own build command,
    // and a build that repoints `current` on every run would otherwise be
    // rebuilt every tick forever, the exact loop `failed` exists to stop.
    let failed = (!landed).then(|| head.to_owned());

    if state.failed != failed {
        // Re-read before writing, because this process may not be the only
        // one writing this record. `state` here is whatever THIS process
        // read at its own start, and `State::write` replaces the whole
        // record, so writing it blind is last-writer-wins over a record
        // somebody else may have advanced since.
        //
        // Under the record lock from the re-read to the write, so what was
        // re-read is what is written over.
        match lock::hold_record(tree) {
            Err(err) => {
                eprintln!("shep-deploy: {sheep}: could not record what {head} came to: {err}");
            }
            Ok(_record) => match landed_elsewhere(tree, head, failed.as_deref()) {
                Some(fresher) => *state = fresher,
                None => {
                    state.failed = failed;
                    adopt_operator_fields(tree, state);
                    if let Err(err) = state.write(&tree.state_file()) {
                        eprintln!(
                            "shep-deploy: {sheep}: could not record what {head} came to: {err}"
                        );
                    }
                }
            },
        }
    }

    // Both the record in memory and the one on disk. They differ after the
    // one write this module swallows, a verified deploy whose record could
    // not be written: memory names the new sha and disk still names the old
    // one, and the old one is what the next run's rollback looks for.
    let recorded = State::read(&tree.state_file()).ok();
    // `staged` is spared like `deployed`: it is built, recorded, and waiting
    // for a deploy that has not come yet, so it is not `current` and can be
    // older than the newest `keep` by then.
    let spared: Vec<&str> = [
        state.deployed.as_deref(),
        recorded
            .as_ref()
            .and_then(|on_disk| on_disk.deployed.as_deref()),
        state.staged.as_deref(),
        recorded
            .as_ref()
            .and_then(|on_disk| on_disk.staged.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect();
    // `git worktree remove` per doomed release, off the runtime's thread
    // like every other git call.
    let pruned = {
        let (tree, spared): (Tree, Vec<String>) = (
            tree.clone(),
            spared.iter().map(|sha| (*sha).to_owned()).collect(),
        );
        shared::off_thread(move || {
            let spared: Vec<&str> = spared.iter().map(String::as_str).collect();
            retention::prune(&tree, keep, &spared)
        })
        .await
    };
    if let Err(err) = pruned {
        eprintln!("shep-deploy: {sheep}: could not reclaim old releases: {err}");
    }
}

/// Takes `watch`, `verify` and `branch` from the record on disk into `state`,
/// so a deploy's own write of the record does not put back a setting the
/// operator changed while it ran.
///
/// Those fields are the operator's: `set_watch` and `set_branch` write one
/// each and a hand edit writes `verify`, and `watch` is documented as
/// something to change in the middle of an incident. A deploy holds this
/// `State` from its first read to its last write, which can be a build's worth
/// of time, and writing it whole put the operator's `manual` back to `auto` at
/// the end of the very deploy they were trying to stop the next one of.
/// Called before every whole-record write a deploy makes: the verified arm's,
/// the rollback's, and `record`'s. A record that cannot be read leaves `state`
/// as it is: there is nothing to adopt.
fn adopt_operator_fields(tree: &Tree, state: &mut State) {
    if let Ok(on_disk) = State::read(&tree.state_file()) {
        state.watch = on_disk.watch;
        state.verify = on_disk.verify;
        state.branch = on_disk.branch;
    }
}

/// Records `head` as the deployed release, in memory and on disk.
///
/// Clears `staged` in the same write: whatever was staged has landed or has
/// been passed over, and a stale one would be reported as built for a head
/// the branch has moved past.
///
/// Lifts the hold in the same write. A retry of a held commit that lands has
/// `failed` naming the very sha going into `deployed`, and a record that says
/// both is refused by [`State::write`], so the one write meant to record the
/// deploy was the one that failed.
///
/// # Errors
/// Whatever [`write_record`] returns.
pub(super) fn record_landing(tree: &Tree, state: &mut State, head: &str) -> Result<(), Error> {
    state.deployed = Some(head.to_owned());
    let held = state.failed.take();
    let staged = state.staged.take();
    // The operator's fields from disk, not this deploy's copy of them: see
    // `write_record`.
    let written = write_record(tree, state);
    if written.is_err() {
        // The hold is still on disk, so memory keeps saying so: `record`
        // compares the two to decide whether it has a write to make, and
        // would otherwise find nothing to do.
        state.failed = held;
        state.staged = staged;
    }
    written
}

/// Writes `state` whole, under the record lock, with the operator's fields
/// taken from disk first.
///
/// The lock is what makes the adopt-then-write one step: without it a
/// `set_watch` could land between the read `adopt_operator_fields` makes
/// and the rename this makes, and be renamed over. Every whole-record
/// write a deploy makes goes through here.
///
/// # Errors
/// [`Error::Io`] if the record lock cannot be taken, and whatever
/// [`State::write`] returns.
pub(crate) fn write_record(tree: &Tree, state: &mut State) -> Result<(), Error> {
    let _record = lock::hold_record(tree)?;
    adopt_operator_fields(tree, state);
    state.write(&tree.state_file())
}

/// The on-disk record, when it already says `head` is deployed and this
/// attempt is about to say `head` failed.
///
/// The poll loop guarantees one tick never overlaps another IN THIS PROCESS.
/// It says nothing about a second process, and README.md tells an operator to
/// start one: `shep-deploy deploy <sheep>` is the documented way to retry a
/// held commit. Run while the dog's own tick is deploying the same sha, both
/// processes read `deploy.toml` at their own start and write it back whole.
///
/// Round 10 of the founder's review reproduced where that lands. The straggler
/// loses its checkout to git's own index lock, fails, and writes its record:
/// `deployed` reverts to the sha it read at startup and `failed` names the sha
/// that is actually running. `go` then refuses to retry that sha forever,
/// because `state.failed` matching the branch head is exactly the hold
/// condition, so auto-deploy stops until somebody notices.
///
/// Adopting the fresher record rather than refusing to write is what makes the
/// in-memory copy stop being stale too: the poll loop keeps this `State` for
/// the rest of its life.
///
/// Written when the two-process race was open, which it is not any more: `go`
/// takes an exclusive `flock` on the tree before anything below it runs, so two
/// processes cannot both be inside a deploy of one sheep. See `crate::lock`.
///
/// Kept anyway, because the lock is per tree and taken by `go`, and this guard
/// is not. Anything writing the record WITHOUT going through `go` still races:
/// `set_watch` does, by design, so that `--watch manual` lands during a deploy
/// rather than waiting an hour for its lock; see [`adopt_operator_fields`] for
/// the other half of that arrangement, and `lock::hold_record` for the lock
/// both sides now hold across their read and write. One read that refuses to
/// overwrite a fresher record is cheap enough to keep under it.
fn landed_elsewhere(tree: &Tree, head: &str, failing: Option<&str>) -> Option<State> {
    failing?;
    let on_disk = State::read(&tree.state_file()).ok()?;
    (on_disk.deployed.as_deref() == Some(head)).then_some(on_disk)
}

/// Sets `watch` on the target and records it, without deploying.
///
/// The verb an operator reaches this through is `deploy`, so it is worth
/// being explicit: this function takes no [`crate::daemon::Daemon`] at all, and therefore
/// cannot start, reload or restart anything even by accident. `manual` is
/// what somebody sets in the middle of an incident, and a deploy firing at
/// that moment is the exact opposite of what they asked for.
///
/// `auto` is refused on a tree the cutover never landed on, and `manual` is
/// allowed in every state. The asymmetry is the same one [`super::deploy`]
/// draws in `go`: such a target has served nothing, a deploy against it would
/// report success for a release nothing ran, and `auto` is the setting that
/// asks for that deploy unattended, once an interval, forever.
///
/// # Why this does NOT take the deploy lock
///
/// `manual` is what somebody sets in the middle of an incident, and a
/// deploy holds the tree's lock from its fetch through its build, which the
/// default `build_timeout` allows an hour for. A `--watch manual` refused as
/// `AlreadyDeploying` for that hour is the opposite of what the setting is
/// for. So this writes without the tree lock, under the record lock instead
/// (`lock::hold_record`), which every whole-record write a deploy makes
/// also takes: the read here and the write here are one step nothing can
/// land between, and the deploy's own writes take `watch` and `verify`
/// from the disk rather than from their stale copy, see
/// `adopt_operator_fields`, so the setting survives them. Before the record
/// lock there was a window the width of one read and one write, in which a
/// deploy's rename could drop this one or this one drop a `deployed` the
/// deploy had just recorded.
///
/// # Errors
/// [`Error::Config`] if `watch` is `auto` and the target's record names no
/// released sha. [`Error::Io`] or [`Error::Config`] if `deploy.toml` cannot
/// be read or written - see [`State::read`] and [`State::write`].
///
/// Answers the `watch` the record carried before, and the record as it is
/// after: the read is this function's, so a caller has nothing older to
/// print from.
pub fn set_watch(tree: &Tree, watch: Watch) -> Result<(Watch, State), Error> {
    let _record = lock::hold_record(tree)?;
    let mut state = State::read(&tree.state_file())?;
    let was = state.watch;

    if watch == Watch::Auto && state.deployed.is_none() {
        return Err(Error::Config(format!(
            "{} has a deploy tree but was never cut over to it, so there is nothing for the \
             poll loop to deploy: its record names no released sha, and nothing has ever been \
             served from that tree. Watching it would deploy at every interval and each of \
             those would be refused. Finish the cutover instead - remove {} and run \
             `shep-deploy setup {}`.",
            tree.sheep(),
            tree.root().display(),
            tree.sheep()
        )));
    }

    state.watch = watch;
    state.write(&tree.state_file())?;
    Ok((was, state))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use crate::deploy::test_support::{
        Shepherd, commit_on_origin, fixture_before_any_release, fixture_with_previous_release,
        install_release, test_config_keeping,
    };
    use crate::deploy::{Outcome, deploy, unattended};
    use crate::error::Error;
    use crate::paths::Tree;
    use crate::state::{State, Watch};
    use crate::{fixtures, lock, swap};

    use super::*;

    /// fails if a straggler process can mark a sha failed that another
    /// process already deployed.
    ///
    /// Two processes on one tree is not exotic: README.md documents
    /// `shep-deploy deploy <sheep>` as the way to retry a held commit, and the
    /// poll loop only guarantees a tick does not overlap another tick IN THE
    /// SAME PROCESS. Both read `deploy.toml` at their own start, and
    /// `State::write` replaces the whole record.
    ///
    /// Modelled with the two independent reads that produces. One clone runs
    /// a real deploy to completion, so the file says `second` is deployed. The
    /// other is still holding what it read at startup, and records a failure
    /// for the same sha, which is what a process that lost the checkout to
    /// git's own index lock does.
    ///
    /// The damage is not just a wrong line in a file. `go` refuses to retry a
    /// sha that `state.failed` names, so a record saying the running release
    /// failed freezes auto-deploy for that target until somebody notices.
    #[tokio::test(start_paused = true)]
    async fn a_straggler_cannot_fail_a_sha_another_process_deployed() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        let mut straggler = fixture.state.clone();

        deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("the winner lands");

        record(
            &fixture.tree,
            &mut straggler,
            &second,
            &Err(Error::Protocol("lost the checkout race".to_owned())),
            fixtures::dog_config().retention,
        )
        .await;

        let written = State::read(&fixture.tree.state_file()).expect("still readable");
        assert_eq!(
            written.deployed.as_deref(),
            Some(second.as_str()),
            "the record must still name the sha that is actually running"
        );
        assert_eq!(
            written.failed, None,
            "a sha another process deployed must not be recorded as failed"
        );
    }

    /// fails if a sha whose deploy did not land is not written down. The
    /// poll loop is why: `state.deployed` advances only on a verify, so
    /// without a second record the loop compares the branch head against
    /// the release that is still serving, finds them different, and runs
    /// the whole sequence again - fetch, full rebuild, swap, reload, wait
    /// out the verification budget, roll back, reload again - every
    /// interval, forever, from one bad commit.
    #[tokio::test(start_paused = true)]
    async fn a_sha_that_did_not_land_is_written_down() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = unattended(
            &Shepherd::never_ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("rolls back");

        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        assert_eq!(fixture.state.failed.as_deref(), Some(second.as_str()));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml was written")
                .failed
                .as_deref(),
            Some(second.as_str()),
            "and it survives a restart of the dog"
        );
    }

    /// fails if a target that never verifies accumulates a release
    /// directory per commit. Retention used to run only on the verified
    /// path, so the one target that keeps failing - the one building a
    /// fresh worktree every time somebody pushes - was the one nothing
    /// ever reclaimed, and it grew with the branch rather than with
    /// `retention`.
    ///
    /// The release that just failed is the newest, so it is one of the
    /// ones retention keeps. What this pins is the bound: the newest
    /// `keep`, plus whatever `current` names, and nothing else.
    #[tokio::test(start_paused = true)]
    async fn failed_releases_are_reclaimed_like_any_other() {
        let mut fixture = fixture_with_previous_release();
        let live = fixture.state.deployed.clone().expect("a previous release");
        let mut failed = Vec::new();
        for name in ["second.txt", "third.txt", "fourth.txt"] {
            failed.push(commit_on_origin(&fixture, name));
            unattended(
                &Shepherd::never_ready(),
                &fixture.tree,
                &mut fixture.state,
                &test_config_keeping(2),
            )
            .await
            .expect("rolls back");
        }

        assert!(fixture.tree.release(&live).exists(), "current is spared");
        assert!(
            !fixture.tree.release(&failed[0]).exists(),
            "the oldest failure is reclaimed"
        );
        assert!(fixture.tree.release(&failed[2]).exists(), "the newest kept");
        let count = fs::read_dir(fixture.tree.releases())
            .expect("reads")
            .count();
        assert_eq!(count, 3, "the newest two, plus what current names");
    }

    /// fails if a `--watch` and a deploy's record write can drop each
    /// other's update. Staged exactly: the record lock held as a deploy
    /// holds it, `set_watch` waiting on another thread, a fresher
    /// `deployed` written meanwhile, and after the release the record has
    /// to carry both the fresh sha and the operator's `manual`. Before the
    /// record lock, `set_watch`'s snapshot renamed over the fresh one.
    #[test]
    fn a_watch_set_while_a_deploy_writes_the_record_loses_nothing() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        fs::create_dir_all(tree.root()).expect("tree");
        let mut state = State {
            deployed: Some(fixtures::SHA.to_owned()),
            ..fixtures::state()
        };
        state.write(&tree.state_file()).expect("the record before");

        let held = lock::hold_record(&tree).expect("the deploy's hold");
        let (tx, rx) = std::sync::mpsc::channel();
        let operator = {
            let home = home.path().to_owned();
            std::thread::spawn(move || {
                let result = set_watch(&Tree::for_sheep(&home, "web"), Watch::Manual);
                tx.send(()).expect("report");
                result
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "set_watch must wait for the record lock"
        );

        // The deploy's write, under its hold.
        state.deployed = Some(fixtures::OTHER_SHA.to_owned());
        state.write(&tree.state_file()).expect("the deploy's write");
        drop(held);

        rx.recv_timeout(Duration::from_secs(5))
            .expect("set_watch lands");
        operator.join().expect("thread").expect("sets");
        let after = State::read(&tree.state_file()).expect("reads");
        assert_eq!(after.deployed.as_deref(), Some(fixtures::OTHER_SHA));
        assert_eq!(after.watch, Watch::Manual);
    }

    /// fails if a race stops being held. With the tree lock, `current`
    /// moving under a deploy means an operator's hand or the release's own
    /// build command did it, and a build that repoints `current` on every
    /// run would be rebuilt every tick forever if its sha were not held.
    #[tokio::test]
    async fn a_raced_deploy_is_held_like_any_other_failure() {
        let fixture = fixture_with_previous_release();
        let mut state = fixture.state.clone();
        let raced: Result<Outcome, Error> = Err(Error::Raced {
            sheep: "web".to_owned(),
            started: "a".to_owned(),
            found: "b".to_owned(),
        });

        record(&fixture.tree, &mut state, fixtures::OTHER_SHA, &raced, 5).await;

        assert_eq!(state.failed.as_deref(), Some(fixtures::OTHER_SHA));
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("reads")
                .failed
                .as_deref(),
            Some(fixtures::OTHER_SHA)
        );
    }

    /// fails if a deploy that LANDS puts back a `watch` the operator changed
    /// while it ran. The deploy holds its `State` from its first read to
    /// its last write, and the verified arm's write of it whole put a
    /// mid-deploy `--watch manual` back to `auto`: the exact deploy the
    /// operator was trying to make the last one for a while.
    #[tokio::test(start_paused = true)]
    async fn a_landed_deploy_keeps_the_watch_the_operator_set_meanwhile() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.watch = Watch::Auto;
        fixture
            .state
            .write(&fixture.tree.state_file())
            .expect("watched");
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::flipping_watch(fixture.tree.state_file()),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("lands");

        assert!(matches!(outcome, Outcome::Deployed { .. }), "{outcome:?}");
        let written = State::read(&fixture.tree.state_file()).expect("reads");
        assert_eq!(
            written.watch,
            Watch::Manual,
            "the operator's setting survives"
        );
        assert_eq!(
            written.deployed.as_deref(),
            Some(second.as_str()),
            "and the deploy is recorded by the same write"
        );
    }

    /// fails if `record`'s own write, the one a FAILED deploy makes, puts
    /// back a `watch` the operator changed while it ran. The landed case
    /// has its own test above; this drives `record` directly.
    #[tokio::test]
    async fn a_failed_deploys_record_write_keeps_the_watch_the_operator_set_meanwhile() {
        let fixture = fixture_with_previous_release();
        // The copy a deploy holds from its first read: watched.
        let mut in_flight = fixture.state.clone();
        in_flight.watch = Watch::Auto;

        // The operator, mid-deploy.
        let mut on_disk = fixture.state.clone();
        on_disk.watch = Watch::Manual;
        on_disk
            .write(&fixture.tree.state_file())
            .expect("--watch manual");

        let failed: Result<Outcome, Error> = Err(Error::Build { status: Some(1) });
        record(
            &fixture.tree,
            &mut in_flight,
            fixtures::OTHER_SHA,
            &failed,
            5,
        )
        .await;

        let written = State::read(&fixture.tree.state_file()).expect("reads");
        assert_eq!(
            written.watch,
            Watch::Manual,
            "the operator's setting survives"
        );
        assert_eq!(
            written.failed.as_deref(),
            Some(fixtures::OTHER_SHA),
            "and the deploy's own fact is recorded"
        );
    }

    /// fails if the release the record on DISK names can be reclaimed after
    /// a verified deploy whose record could not be written. Memory names the
    /// new sha then, disk still names the old one, and the old one is what
    /// the next run rolls back to.
    #[tokio::test]
    async fn the_release_the_disk_record_names_is_spared_after_a_lost_write() {
        let mut fixture = fixture_with_previous_release();
        let old = fixture.state.deployed.clone().expect("a previous release");
        // Three newer releases, all fresher than `old`, so that with
        // `keep = 2` the old one is a candidate unless something spares it.
        let mut newest = String::new();
        for n in 0..3 {
            newest = commit_on_origin(&fixture, &format!("release-{n}.txt"));
            crate::git::fetch(
                &fixture.tree.git(),
                fixture.origin.path().to_str().expect("utf-8"),
                fixtures::TEST_BUDGET,
            )
            .expect("fetch");
            install_release(&fixture, &newest);
        }
        // Memory has moved on; disk has not.
        fixture.state.deployed = Some(newest.clone());

        let landed: Result<Outcome, Error> = Ok(Outcome::Deployed { sha: newest });
        record(&fixture.tree, &mut fixture.state, "unused", &landed, 2).await;

        assert!(
            fixture.tree.release(&old).is_dir(),
            "the release the disk record names must survive"
        );
    }

    /// fails if the write that records a landing is refused when the sha it
    /// lands was held. A retry of a held commit that succeeds has `failed`
    /// naming the sha it is about to put in `deployed`, and `State::write`
    /// refuses a record that says both, so the record of the deploy was lost
    /// and an error printed over a deploy that had worked.
    #[tokio::test(start_paused = true)]
    async fn landing_a_held_sha_records_it_and_lifts_the_hold() {
        let mut fixture = fixture_with_previous_release();
        let head = commit_on_origin(&fixture, "second.txt");
        fixture.state.failed = Some(head.clone());
        fixture
            .state
            .write(&fixture.tree.state_file())
            .expect("the hold is on disk");

        record_landing(&fixture.tree, &mut fixture.state, &head)
            .expect("the landing is recorded at the first attempt");

        let written = State::read(&fixture.tree.state_file()).expect("reads");
        assert_eq!(written.deployed.as_deref(), Some(head.as_str()));
        assert_eq!(written.failed, None, "a landed sha is no longer held");
    }

    /// fails if a landing whose write is lost forgets the hold it was
    /// lifting. `record` decides whether it has a write to make by comparing
    /// memory with the hold the outcome implies, and a landed sha implies
    /// none, so only a `failed` still in memory makes it retry the write.
    /// A directory at the record's path is what makes the rename fail here.
    #[tokio::test(start_paused = true)]
    async fn a_lost_landing_write_keeps_the_hold_in_memory_for_record_to_retry() {
        let mut fixture = fixture_with_previous_release();
        let head = commit_on_origin(&fixture, "second.txt");
        fixture.state.failed = Some(head.clone());
        let record = fixture.tree.state_file();
        fs::remove_file(&record).expect("drop the record");
        fs::create_dir(&record).expect("a directory where the record goes");

        record_landing(&fixture.tree, &mut fixture.state, &head)
            .expect_err("the record cannot be written");

        assert_eq!(
            fixture.state.failed.as_deref(),
            Some(head.as_str()),
            "the hold still stands on disk, so memory still says so"
        );
    }

    /// fails if a record that cannot be written after a verified deploy
    /// turns the deploy into a failure. The release is up and serving, so
    /// the outcome is `Deployed`; returning the write's error instead had
    /// `record` write the sha into `failed` beside the same sha in
    /// `deployed`. A directory at the record's path is what makes the
    /// rename fail here.
    #[tokio::test(start_paused = true)]
    async fn a_record_that_cannot_be_written_still_reports_deployed() {
        let mut fixture = fixture_with_previous_release();
        let head = commit_on_origin(&fixture, "second.txt");
        let record = fixture.tree.state_file();
        fs::remove_file(&record).expect("drop the record");
        fs::create_dir(&record).expect("a directory where the record goes");

        let outcome = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("the deploy landed whatever the record did");

        assert_eq!(outcome, Outcome::Deployed { sha: head.clone() });
        assert_eq!(fixture.state.deployed.as_deref(), Some(head.as_str()));
        assert_eq!(fixture.state.failed, None, "a landed sha is never held");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&head))
        );
    }

    /// fails if setting the watch mode also deploys. `--watch manual` is
    /// what an operator reaches for during an incident, and a deploy firing
    /// at that moment is the exact opposite of what they asked for. The
    /// verb says deploy, which is why this needs a test rather than a
    /// comment.
    ///
    /// The brief's version of this test asserted on a recording daemon's
    /// call list. [`set_watch`] takes no daemon at all, so that assertion
    /// cannot be written - and its absence is the stronger guarantee: no
    /// daemon traffic is a type error here, not a test failure. What is
    /// left to check is the filesystem half, which a fall-through could
    /// still touch: no new release, `current` unmoved, `deployed`
    /// unchanged.
    #[tokio::test]
    async fn setting_the_watch_mode_does_not_deploy() {
        let fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        let second = commit_on_origin(&fixture, "second.txt");

        let (was, after) = set_watch(&fixture.tree, Watch::Manual).expect("sets");

        assert_eq!(was, fixture.state.watch, "what the record carried before");
        assert_eq!(after.watch, Watch::Manual);
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml was written")
                .watch,
            Watch::Manual
        );
        assert!(
            !fixture.tree.release(&second).exists(),
            "no release may be built"
        );
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.tree.release(&previous))
        );
        assert_eq!(after.deployed.as_deref(), Some(previous.as_str()));
    }

    /// fails if a tree the cutover never landed on can be put under the
    /// poll loop. `deploy` refuses such a target, so `auto` here arms a
    /// target that can only be refused, once an interval, for as long as
    /// the abandoned tree sits there - and it is the last verb that
    /// treated one as ordinary. `manual` stays allowed in every state,
    /// because it is the direction out.
    #[tokio::test]
    async fn a_tree_the_cutover_never_landed_on_cannot_be_watched() {
        let fixture = fixture_before_any_release();

        let err = set_watch(&fixture.tree, Watch::Auto).expect_err("refuses");

        let shown = err.to_string();
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("shep-deploy setup web"), "{shown}");
        assert_eq!(
            State::read(&fixture.tree.state_file())
                .expect("deploy.toml still reads")
                .watch,
            Watch::Manual,
            "unchanged on disk"
        );

        set_watch(&fixture.tree, Watch::Manual).expect("manual is allowed");
    }

    /// fails if a prune failure fails the deploy that triggered it. The
    /// `Landed::Verified` arm's `if let Err` exists precisely so that this
    /// cannot happen: a deploy that verified and wrote its record already
    /// succeeded, and a worktree that will not delete costs disk, not
    /// correctness.
    ///
    /// The failure is real, not simulated: `first`'s worktree admin
    /// bookkeeping is removed out from under it, so `git worktree remove
    /// --force` on the directory that is still sitting there answers
    /// "is not a working tree" - the same failure an operator's own `rm -rf`
    /// on a release directory would leave behind.
    #[tokio::test(start_paused = true)]
    async fn a_prune_failure_does_not_fail_the_deploy() {
        let mut fixture = fixture_with_previous_release();
        let first = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("second release verifies");

        fs::remove_dir_all(fixture.tree.git().join("worktrees").join(&first))
            .expect("corrupt first's worktree bookkeeping");

        commit_on_origin(&fixture, "third.txt");
        let outcome = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &test_config_keeping(2),
        )
        .await
        .expect("a prune failure must not fail the deploy");

        assert!(matches!(outcome, Outcome::Deployed { .. }), "{outcome:?}");
    }
}
