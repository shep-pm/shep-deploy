//! Part two of opt-in: registering the sheep against `current` and removing
//! the instances it replaced.

use shep_client::shep_core::protocol::ProcessInfo;

use super::attempt::{CutOver, attempt};
use super::prepare::Prepared;
use super::undo::undo_start;
use crate::daemon::Daemon;
use crate::error::Error;
use crate::lock;
use crate::state::{State, Watch};
use crate::verify::Generation;

/// Registers the sheep against `current`, waits for the newcomer to start
/// and stay up, and deletes the instances it replaced.
///
/// # Errors
/// A cutover that lands with nothing stranded promotes the record's `watch`
/// from the `manual` [`super::prepare()`] wrote to `auto`, which is what makes the
/// target the poll loop's. A stranded one does not: instances it could not
/// delete are still registered, and every later deploy reloads the name and
/// respawns each of them from its own pre-adoption spec, so a loop polling
/// it would bring them back on a schedule. Remove the ids the error names,
/// then `shep deploy <sheep> --watch auto`.
///
/// A `verify` the operator edited in the record while the cutover ran is
/// kept; a `--watch manual` typed while it ran is not, because the record
/// already said `manual` from [`super::prepare()`] and the two cannot be told
/// apart. Run it after setup returns.
///
/// # Errors
/// [`Error::CutOver`] if the newcomer never came up, in which case it has
/// been removed and the original is still serving, untouched. That error
/// also carries whether the shepherd's persisted record could be put back:
/// see [`super::undo::undo_start`] for why deleting the newcomer is not enough on its
/// own, and why a failure there is something an operator cannot see in
/// `shep flock` and has to be told by name.
///
/// A refused `Start` is different in kind and is returned unchanged.
/// Nothing was spawned and, just as importantly, nothing was recorded, so
/// the machine really is exactly as it was. The [`Daemon::describe`] before
/// the `Start` is in the same position, and its error is passed through for
/// the same reason.
///
/// Two steps run only once the newcomer has been accepted, and they are
/// not alike.
///
/// [`Error::Stranded`] is a [`Daemon::delete`] that failed. The cutover
/// landed and the new release is serving, but an instance it replaced is
/// still registered, and that does NOT settle: a later deploy reloads every
/// instance of the name and respawns each from its own spec, so the
/// leftover comes back on the pre-adoption config every time. The ids are
/// collected rather than the first failure returned, so the error can name
/// every one of them.
///
/// [`State::write`] failing is the mild one, and it is passed through. The
/// release is serving and only the record is missing, which the next
/// `shep-deploy deploy` writes.
///
pub async fn cut_over<D: Daemon>(daemon: &D, prepared: Prepared) -> Result<String, Error> {
    let (hold, tree, mut state, sha, app, previous_config) = prepared.into_parts();
    // Held to the end of this function, and named so it is not dropped here.
    let _hold = hold;
    let sheep = tree.sheep().to_owned();

    // Captured BEFORE the Start, and this is the only moment it can be:
    // afterwards two rows share this name and nothing on the wire says
    // which of them was already here. Reading it later would risk deleting
    // the release that was just deployed.
    //
    // One read, not two. The ids to delete and the pids to compare against
    // have to describe the same instant: asking twice would let an instance
    // restart between them, so the generation would be frozen against a set
    // of pids the id list never named. That is the whole argument
    // `Generation::of_infos` was added for, and it applies to its own
    // caller first.
    let before = daemon.describe(&sheep).await?;
    let rows: Vec<&ProcessInfo> = before.iter().collect();
    let previous: Vec<u32> = rows.iter().map(|info| info.id).collect();
    let generation = Generation::of_infos(&rows);

    // THE line this task exists for. Over the socket a `cwd` of None stays
    // None and the child inherits the SHEPHERD's working directory, and a
    // Flockfile registered by the CLI from inside a release is canonicalised
    // to that release and pinned there. An explicit one is stored verbatim,
    // symlink and all, which is the only spelling that follows a swap.
    let mut registering = app;
    registering.cwd = Some(tree.current().display().to_string());
    // The interpreter the sheep already runs under, when the Flockfile names
    // none. `shep start` fills an unset `interpreter` from `shep.toml`'s
    // `[interpreters]` by the script's extension, and a fresh home maps
    // `js`, `py`, `rb`, `sh` and more out of the box; the shepherd itself
    // never does, and runs a script with no interpreter directly. This dog
    // cannot read that map, so a Flockfile that relies on it would have
    // been registered here with none and the replacement exec'd `server.js`
    // as a program. The registered value IS the map's answer for this
    // sheep, so it is carried. An explicit value, `"none"` included,
    // outranks the map for the CLI and is left alone here for the same
    // reason. Read out of shep-cli's `apply_interpreters` on 2026-09-04.
    if registering.interpreter.is_none() {
        registering.interpreter = previous_config.interpreter.clone();
    }

    match attempt(daemon, &sheep, registering, &generation, &previous).await {
        CutOver::Done => {
            // By id, because Start added the newcomer BESIDE these under
            // the same name and a name selector would take it down too.
            //
            // Collected rather than `?`d. A delete that failed partway
            // leaves instances that a later deploy does not clean up but
            // RELOADS, respawning each from its own pre-adoption spec, so
            // the ids have to reach the operator. Stopping at the first
            // failure would hide the rest of them.
            let mut stranded = Vec::new();
            for id in previous {
                if daemon.delete(id).await.is_err() {
                    stranded.push(id);
                }
            }

            // Written either way: the release is serving, so the record
            // naming it is true whatever the cleanup did.
            state.deployed = Some(sha.clone());
            // The promotion out of `manual`, and only on the clean path.
            // The tree has now been served from, which is what makes it a
            // deploy target at all. A cutover that left instances behind
            // has not finished: every later deploy reloads the name and
            // respawns each leftover from its own pre-adoption spec, so a
            // loop polling this target would bring them back on a
            // schedule. The operator removes the ids the error names and
            // then asks for `--watch auto`.
            if stranded.is_empty() {
                state.watch = Watch::Auto;
            }
            let outcome = if stranded.is_empty() {
                Ok(sha)
            } else {
                Err(Error::Stranded {
                    sheep: sheep.clone(),
                    sha,
                    ids: stranded,
                })
            };
            // The ids outrank the write. A record that could not be written
            // costs one more `shep deploy`; an id that never reached the
            // operator costs an instance respawned on the old config on every
            // deploy from now on, so the write's failure is printed rather
            // than allowed to replace the error that names them.
            //
            // `verify` is the operator's, hand-edited in the record, and a
            // first build can take an hour: an edit made during it is taken
            // from the disk rather than written over by this process's copy
            // from `prepare`. `watch` is deliberately NOT taken from the
            // disk. `prepare` wrote `manual` so the poll loop leaves the
            // tree alone until it has been served from, and the line above
            // is the promotion out of that; a `--watch manual` typed during
            // setup reads the same as the `manual` `prepare` wrote, and is
            // overwritten. Setup is a command the operator is sitting at;
            // `--watch manual` after it returns is the way to keep it.
            let written = lock::hold_record(&tree).and_then(|_record| {
                if let Ok(on_disk) = State::read(&tree.state_file()) {
                    state.verify = on_disk.verify;
                }
                state.write(&tree.state_file())
            });
            if let Err(err) = written {
                if outcome.is_ok() {
                    return Err(err);
                }
                eprintln!("shep-deploy: {sheep}: could not record the cutover: {err}");
            }
            outcome
        }
        // Nothing was spawned and nothing was recorded, so there is nothing
        // to clean up and nothing to say beyond what the shepherd said.
        CutOver::NotStarted(source) => Err(source),
        CutOver::NotVerified(why) => {
            let undone = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver {
                sheep,
                why,
                removed: undone.removed,
                repaired: undone.recorded,
                tree: tree.root().to_owned(),
                source: None,
            })
        }
        // Reported the same way rather than returned bare. This arm is
        // reached because a request failed, which means the shepherd went
        // quiet, which is exactly when `undo_start`'s own opening
        // `describe` fails too. So it is the arm most likely to leave a
        // poisoned roll and the least able to repair one, and a bare
        // transport error would tell the operator nothing about that.
        CutOver::Failed(source) => {
            let undone = undo_start(daemon, &sheep, &previous, previous_config).await;
            Err(Error::CutOver {
                sheep,
                why: SHEPHERD_QUIET.to_owned(),
                removed: undone.removed,
                repaired: undone.recorded,
                tree: tree.root().to_owned(),
                source: Some(Box::new(source)),
            })
        }
    }
}

/// The `why` for a cutover that ended because the shepherd stopped
/// answering, rather than because the release failed.
const SHEPHERD_QUIET: &str = "The shepherd stopped answering while the new instance was being \
     watched, so nothing was established about it either way.";

#[cfg(test)]
mod tests {
    // For `Error::source` on the variant the quiet-shepherd path returns.
    use core::error::Error as _;

    use super::*;
    use crate::optin::test_support::{
        NEWCOMER_ID, cutover_fixture, cutover_fixture_comes_up_but_refuses_deletes,
        cutover_fixture_dies_during_dwell, cutover_fixture_never_appears,
        cutover_fixture_refusing_start, cutover_fixture_shepherd_goes_quiet,
        cutover_fixture_with_instances,
    };
    use crate::state::Verify;
    /// fails if the new registration's cwd is anything but the `current`
    /// symlink, set EXPLICITLY. A Flockfile app's DEFAULTED cwd is resolved
    /// at registration, so registering from inside a release pins the sheep
    /// to that release and every later swap moves a symlink the app no
    /// longer reaches. Measured on a real shepherd: the reload after a swap
    /// re-ran the OLD release's script. This is the single most
    /// load-bearing line in the crate and it fails silently, one release
    /// later, when it is wrong.
    #[tokio::test(start_paused = true)]
    async fn the_new_registration_names_current_explicitly() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let current = prepared.tree.current();
        cut_over(&daemon, prepared).await.expect("cuts over");

        let started = daemon.started();
        assert_eq!(started.len(), 1, "exactly one Start");
        assert_eq!(
            started[0].cwd.as_deref(),
            current.to_str(),
            "cwd must be the current symlink itself, not a release and not None"
        );
    }

    /// fails if a `verify` the operator edited during the cutover is written
    /// over by the cutover's own copy from `prepare`. A first build can take
    /// an hour, and the record is the file they are told to edit.
    #[tokio::test(start_paused = true)]
    async fn a_verify_edited_during_the_cutover_survives_its_record_write() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let path = prepared.tree.state_file();
        let mut edited = State::read(&path).expect("the record prepare wrote");
        assert_eq!(edited.verify, Verify::Probed, "the default before the edit");
        edited.verify = Verify::Alive;
        edited.write(&path).expect("the operator's edit");

        cut_over(&daemon, prepared).await.expect("cuts over");

        let after = State::read(&path).expect("reads");
        assert_eq!(after.verify, Verify::Alive, "the edit survives");
        assert_eq!(after.watch, Watch::Auto, "and the promotion still lands");
        assert!(after.deployed.is_some());
    }

    /// fails if a Flockfile that names no `interpreter` is registered with
    /// none. `shep start` filled the sheep's from `shep.toml`'s
    /// `[interpreters]` by extension, the shepherd itself never does, and
    /// this dog cannot read the map: registered bare, the replacement would
    /// exec `server.js` as a program. An explicit value, `"none"` included,
    /// is the Flockfile's own and stays.
    #[tokio::test(start_paused = true)]
    async fn the_new_registration_keeps_the_interpreter_the_sheep_ran_under() {
        let (daemon, mut prepared, _dirs) = cutover_fixture().await;
        prepared.previous_config.interpreter = Some("node".to_owned());
        prepared.app.interpreter = None;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.started()[0].interpreter.as_deref(), Some("node"));

        let (daemon, mut prepared, _dirs) = cutover_fixture().await;
        prepared.previous_config.interpreter = Some("node".to_owned());
        prepared.app.interpreter = Some("none".to_owned());
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.started()[0].interpreter.as_deref(), Some("none"));
    }

    /// fails if the OLD instances are deleted by name rather than by id, or
    /// if the newcomer is deleted instead. A name selector would take the
    /// new instance down with them, because `Start` added it BESIDE the old
    /// under the same name, and an id read after the `Start` could name the
    /// release that was just deployed.
    #[tokio::test(start_paused = true)]
    async fn only_the_old_instances_are_deleted_and_by_id() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7], "the pre-existing instance's id");
        assert!(
            !daemon.deleted().contains(&NEWCOMER_ID),
            "99 is the newcomer"
        );
    }

    /// fails if a sheep running two instances loses only one of them at
    /// the cutover. Every original is deleted by id once the newcomer is
    /// verified, so a cutover that deleted a single id would leave the rest
    /// serving the pre-adoption checkout indefinitely. The Flockfile here
    /// asks for one instance; the scaled cutover has its own tests.
    #[tokio::test(start_paused = true)]
    async fn every_replaced_instance_is_deleted() {
        let (daemon, prepared, _dirs) = cutover_fixture_with_instances(&[7, 8]).await;
        cut_over(&daemon, prepared).await.expect("cuts over");
        assert_eq!(daemon.deleted(), vec![7, 8]);
    }

    /// fails if a Start the shepherd refused is treated as one it accepted.
    /// Nothing was spawned, so there is nothing to delete, and issuing a
    /// Delete against an id that was never created would either do nothing
    /// or, worse, match something else.
    #[tokio::test(start_paused = true)]
    async fn a_refused_start_deletes_nothing() {
        let (daemon, prepared, _dirs) = cutover_fixture_refusing_start().await;

        let err = cut_over(&daemon, prepared).await.expect_err("refused");

        // The shepherd's own refusal, unchanged. An `Error::CutOver` here
        // would be this crate claiming a cutover was abandoned when none
        // was ever begun.
        assert!(matches!(err, Error::Request(_)), "{err}");
        assert!(daemon.deleted().is_empty());
    }

    /// fails if a delete the shepherd refused is passed over in silence,
    /// or if only the first failure is reported. The cutover landed and the
    /// new release is serving, so it would be easy to call this cosmetic,
    /// and it is not: what is left behind is an instance the next deploy
    /// does not clean up but RELOADS, respawning it from its own
    /// pre-adoption spec, so it serves the operator's checkout code while
    /// the supervisor keeps it alive. That is verbatim the consequence the
    /// "leave the original running" design was reversed over. The ids have
    /// to reach the operator, all of them, with the command that removes
    /// them.
    ///
    /// The record is still written, because the release really is serving.
    #[tokio::test(start_paused = true)]
    async fn a_refused_delete_names_every_instance_it_left_behind() {
        let (daemon, prepared, _dirs) = cutover_fixture_comes_up_but_refuses_deletes().await;
        let path = prepared.tree.state_file();
        let sha = prepared.sha.clone();

        let err = cut_over(&daemon, prepared).await.expect_err("names them");

        let shown = err.to_string();
        assert!(shown.contains("shep delete 7"), "{shown}");
        assert!(
            shown.contains("shep delete 8"),
            "not just the first one: {shown}"
        );
        assert_eq!(
            State::read(&path).expect("reads").deployed,
            Some(sha),
            "the release is serving, so the record names it"
        );
    }

    /// fails if a cutover ended by a shepherd that went quiet reports the
    /// transport error and nothing else. This is the arm most likely to
    /// leave the roll poisoned and the least able to repair it: it is
    /// reached BECAUSE a request failed, and `undo_start` opens with a
    /// `describe` of its own, so the repair almost always fails too. The
    /// benign case, a release that did not come up with a healthy shepherd,
    /// gets careful `repaired` reporting; this one used to get a bare
    /// socket error saying nothing about the record left behind. The
    /// shepherd's own failure still has to reach the operator, so it is
    /// carried as a source rather than dropped.
    #[tokio::test(start_paused = true)]
    async fn a_shepherd_that_goes_quiet_still_reports_the_repair_it_could_not_make() {
        let (daemon, prepared, _dirs) = cutover_fixture_shepherd_goes_quiet().await;

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(
            matches!(
                err,
                Error::CutOver {
                    repaired: false,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.source().is_some(), "the shepherd's own error is kept");
        let shown = err.to_string();
        assert!(shown.contains("stopped answering"), "{shown}");
        assert!(
            shown.contains("muster"),
            "the roll paragraph fires: {shown}"
        );
    }

    /// fails if an abandoned cutover leaves the operator thinking the
    /// target is deployable. `deploy` does not short-circuit on a record
    /// naming no release, so `shep deploy` here reloads the sheep at its
    /// own checkout, sees a real pid turnover, and prints success for a
    /// release nothing served. The failure that produces the false green is
    /// the operator following ordinary advice, so this message has to say
    /// which command NOT to run, not merely omit it.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_says_the_target_is_not_deployable_yet() {
        let (daemon, prepared, _dirs) = cutover_fixture_dies_during_dwell().await;
        let tree = prepared.tree.root().to_owned();

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        let shown = err.to_string();
        assert!(shown.contains("is NOT a deploy target"), "{shown}");
        assert!(shown.contains("Do NOT run `shep deploy bpm`"), "{shown}");
        assert!(shown.contains("shep-deploy setup bpm"), "{shown}");
        // Resolved, not `$SHEP_HOME/deploy/bpm`. That variable is usually
        // unset in an operator's own shell, where the fallback is `~/.shep`,
        // so the unexpanded form makes `rm -rf` target `/deploy/bpm` and do
        // nothing at all. The remedy is the whole point of this message.
        assert!(
            shown.contains(&format!("remove {}", tree.display())),
            "{shown}"
        );
        assert!(!shown.contains("$SHEP_HOME"), "{shown}");
    }

    /// fails if the record is advanced before the newcomer verified.
    /// `deploy.toml` naming a release nothing has served is the same defect
    /// the engine plan spent five rounds removing from the deploy path, and
    /// it must not come back through this one.
    #[tokio::test(start_paused = true)]
    async fn the_record_advances_only_after_the_newcomer_is_online() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;
        let path = prepared.tree.state_file();

        let err = cut_over(&daemon, prepared).await.expect_err("gives up");

        assert!(matches!(err, Error::CutOver { .. }), "{err}");
        assert_eq!(State::read(&path).expect("reads").deployed, None);
    }

    /// fails if a tree the cutover has not landed on is left watched. The
    /// poll loop deploys every `auto` target it finds, and a deploy against
    /// such a tree is refused - so a target abandoned between `prepare` and
    /// `cut_over` would be refused once every interval, forever, for a
    /// target the loop should be ignoring outright. `prepare` writes
    /// `manual` and the cutover promotes it, which is the same rule the
    /// docs already state: a sheep is not a deploy target until a cutover
    /// lands.
    #[tokio::test(start_paused = true)]
    async fn a_prepared_target_is_not_watched_until_the_cutover_lands() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let path = prepared.tree.state_file();

        assert_eq!(
            State::read(&path).expect("reads").watch,
            Watch::Manual,
            "prepared, and nothing has served from the tree yet"
        );

        cut_over(&daemon, prepared).await.expect("cuts over");

        assert_eq!(
            State::read(&path).expect("reads").watch,
            Watch::Auto,
            "the cutover landed, so the poll loop may have it"
        );
    }

    /// fails if a cutover that was abandoned leaves the target watched.
    /// This is the tree nobody ever served from, and the loop must pass
    /// over it rather than meeting `deploy`'s refusal every interval.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_cutover_leaves_the_target_unwatched() {
        let (daemon, prepared, _dirs) = cutover_fixture_never_appears().await;
        let path = prepared.tree.state_file();

        cut_over(&daemon, prepared).await.expect_err("gives up");

        assert_eq!(State::read(&path).expect("reads").watch, Watch::Manual);
    }

    /// fails if a cutover that landed and could not tidy up starts the poll
    /// loop off on its own. The release IS serving, so the record names it -
    /// but instances the cutover could not delete are still registered, and
    /// every later deploy reloads the name and respawns each of them from
    /// its own pre-adoption spec. Unattended, that is the leftover coming
    /// back on a schedule. The operator removes the ids the error names and
    /// then asks for `--watch auto`.
    #[tokio::test(start_paused = true)]
    async fn a_stranded_cutover_does_not_start_the_loop_off_on_its_own() {
        let (daemon, prepared, _dirs) = cutover_fixture_comes_up_but_refuses_deletes().await;
        let path = prepared.tree.state_file();

        let err = cut_over(&daemon, prepared).await.expect_err("is stranded");

        assert!(matches!(err, Error::Stranded { .. }), "{err}");
        let state = State::read(&path).expect("reads");
        assert!(state.deployed.is_some(), "the release is serving");
        assert_eq!(state.watch, Watch::Manual);
    }

    /// fails if a cutover that DID land stops recording what it landed, or
    /// answers with a sha that is not the one under `current`. The negative
    /// is pinned next door and the positive was not, so both lines writing
    /// the record could be deleted with the suite still green - and the
    /// state they leave, a cut-over sheep whose record names no release, is
    /// what makes `shep deploy` report success for a release nothing served.
    #[tokio::test(start_paused = true)]
    async fn the_record_and_the_answer_name_the_release_that_was_cut_over() {
        let (daemon, prepared, _dirs) = cutover_fixture().await;
        let path = prepared.tree.state_file();
        let expected = prepared.sha.clone();

        let sha = cut_over(&daemon, prepared).await.expect("cuts over");

        assert_eq!(sha, expected, "the sha it answers with");
        assert_eq!(State::read(&path).expect("reads").deployed, Some(expected));
    }
}
