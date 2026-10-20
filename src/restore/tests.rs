//! Tests for the removal restore: what each sheep becomes and what the report says.

use std::fs;
use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use super::*;
use crate::fixtures;
use crate::paths::Tree;
use crate::restore::test_support::{
    Recording, Refuse, write_record, write_target_never_cut_over, write_target_with_full_origin,
    write_target_with_origin, write_target_with_origin_absent,
};
use crate::state::State;

/// fails if a `deploy.toml` that cannot be parsed is skipped silently.
///
/// This is the one moment the record matters most: removal is when the dog
/// puts every sheep back where its operator will look for it, and a target
/// it cannot read is a target it cannot restore. Skipping it would leave
/// an app running from a path under `$SHEP_HOME` with nothing said about
/// it.
///
/// `poll/tests.rs` pins the identical shape for `tick` in
/// `a_record_that_cannot_be_read_is_reported_rather_than_skipped`; the
/// branch here had no equivalent, so a change to `State::read`'s error
/// path could have gone quiet on one side and been caught only on the
/// other.
#[tokio::test]
async fn a_record_that_cannot_be_read_is_reported_rather_than_skipped() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "garbled");
    std::fs::create_dir_all(tree.root()).expect("create the tree");
    std::fs::write(tree.state_file(), "this is not toml").expect("write deploy.toml");

    let results = all(&Recording::new(&[], Refuse::Never), home.path()).await;

    assert_eq!(results.len(), 1, "the target must be reported, not skipped");
    assert!(
        matches!(&results[0], Restored::Failed { sheep, .. } if sheep == "garbled"),
        "an unreadable record must be a reported failure, got: {:?}",
        results[0]
    );
}

/// fails if removal puts back only `cwd` and `script`. The record
/// carries the app as the shepherd had it before adoption; a restore
/// from the shepherd's current definition kept the deployed
/// repository's `env`, `instances` and probes past the dog's removal.
#[tokio::test]
async fn a_sheep_is_restored_to_its_whole_pre_adoption_definition() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin: AppConfig = toml::from_str(
        "name = \"web\"\nscript = \"server.js\"\ncwd = \"/srv/web\"\ninstances = 3\n\
         [env]\nPORT = \"8080\"\n",
    )
    .expect("an app");
    write_target_with_full_origin(home.path(), "web", &origin);
    let daemon = Recording::new(&["web"], Refuse::Never);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(&results[0], Restored::Returned { .. }),
        "{results:?}"
    );
    let started = daemon.started();
    let put_back = started.first().expect("one Start");
    assert_eq!(put_back.instances, 3, "instances come back");
    assert_eq!(
        put_back.env.get("PORT").map(String::as_str),
        Some("8080"),
        "env comes back"
    );
    assert_eq!(put_back.cwd.as_deref(), Some("/srv/web"));
    assert_eq!(put_back.script, "server.js");
}

/// fails if the report names a directory the sheep was not put back in.
/// `put_back` starts the origin's own `cwd` when the record carries an
/// origin, and `origin_cwd` is a hand-editable legacy field that can
/// disagree with it; the report has to follow the same choice.
#[tokio::test]
async fn the_report_names_the_origins_cwd_when_the_record_carries_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin: AppConfig =
        toml::from_str("name = \"web\"\nscript = \"server.js\"\ncwd = \"/srv/web\"\n")
            .expect("an app");
    let state = State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        origin_cwd: Some(PathBuf::from("/srv/stale")),
        origin_script: Some("stale.js".to_owned()),
        checkout: PathBuf::from("/srv/web"),
        origin: Some(origin),
        ..fixtures::state()
    };
    write_record(home.path(), "web", &state);
    let daemon = Recording::new(&["web"], Refuse::Never);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(&results[0], Restored::Returned { to, .. } if to == Path::new("/srv/web")),
        "{results:?}"
    );
    let started = daemon.started();
    assert_eq!(started[0].cwd.as_deref(), Some("/srv/web"));
    assert_eq!(started[0].script, "server.js");
}

/// fails if an origin with no `cwd` is put back with none while the
/// report names the legacy field. `prepare` refuses a sheep without a
/// cwd, so this is a hand-edited record; the legacy field is the next
/// witness, and what is started and what is reported have to agree.
#[tokio::test]
async fn an_origin_without_a_cwd_is_put_back_at_the_legacy_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let origin: AppConfig =
        toml::from_str("name = \"web\"\nscript = \"server.js\"\n").expect("an app");
    assert!(origin.cwd.is_none());
    let state = State {
        deployed: Some(fixtures::OTHER_SHA.to_owned()),
        origin_cwd: Some(PathBuf::from("/srv/web")),
        origin_script: Some("server.js".to_owned()),
        checkout: PathBuf::from("/srv/web"),
        origin: Some(origin),
        ..fixtures::state()
    };
    write_record(home.path(), "web", &state);
    let daemon = Recording::new(&["web"], Refuse::Never);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(&results[0], Restored::Returned { to, .. } if to == Path::new("/srv/web")),
        "{results:?}"
    );
    assert_eq!(daemon.started()[0].cwd.as_deref(), Some("/srv/web"));
}

/// fails if a sheep that pre-existed the dog is not put back where its
/// operator will look for it. This is the whole point: they will go to
/// ~/ReactMap, because that is where they think their app lives, and
/// the cwd it has been running under is a path beneath $SHEP_HOME they
/// have no reason to know about.
#[tokio::test]
async fn a_pre_existing_sheep_goes_back_to_its_own_checkout() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::with_registered(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(matches!(results[0], Restored::Returned { .. }));
    let started = daemon.started();
    assert_eq!(started[0].cwd.as_deref(), Some("/srv/reactmap"));
    assert_eq!(started[0].script, "bun .");
}

/// fails if the restore stops deleting the old registration first. The
/// registered config is what has to change, and `Start` on a registered
/// name ADDS an instance rather than re-registering it, so without the
/// delete the sheep ends up running from both places at once.
#[tokio::test]
async fn the_old_registration_is_removed_before_the_new_one_is_started() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::with_registered(&["bpm"]);

    all(&daemon, home.path()).await;

    assert_eq!(
        daemon.calls(),
        vec!["delete", "start"],
        "deleting after starting would leave two registrations"
    );
}

/// fails if a sheep the dog bootstrapped is deleted, or is left without
/// being told about. Deleting an app because a deploy tool was
/// uninstalled would be much worse than leaving it, and "left running,
/// unchanged" that nobody is told about is indistinguishable from
/// "quietly abandoned somewhere you will not think to look".
#[tokio::test]
async fn a_bootstrapped_sheep_is_left_running_and_named_in_the_report() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin_absent(home.path(), "ctm");
    let daemon = Recording::with_registered(&["ctm"]);

    let results = all(&daemon, home.path()).await;

    assert!(daemon.calls().is_empty(), "nothing is stopped or started");
    let text = report(&results);
    assert!(text.contains("ctm still running from"), "{text}");
    assert!(text.contains("deploy/ctm/current"), "{text}");
}

/// fails if a tree whose cutover never landed is "restored".
///
/// `optin::prepare` writes `origin_cwd` and `origin_script` and then
/// stops. `cut_over` is what actually moves the sheep. So between them a
/// tree exists naming an origin for a sheep that never left its own
/// checkout, and the branch here read those two fields alone: it could
/// not tell that tree from a sheep genuinely cut over.
///
/// Restoring it deletes and restarts a healthy app to put it back where
/// it already is. Worse if the `Start` then fails, because the delete has
/// already landed and the app is simply stopped. `deployed` is the field
/// that says a cutover landed.
#[tokio::test]
async fn a_cutover_that_never_landed_is_not_restored() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_never_cut_over(home.path(), "ctm", "/srv/ctm", "./run.sh");
    let daemon = Recording::with_registered(&["ctm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        daemon.calls().is_empty(),
        "a sheep that never moved must not be stopped or started: {:?}",
        daemon.calls()
    );
    let text = report(&results);
    assert!(text.contains("ctm was never moved"), "{text}");
    assert!(text.contains("/srv/ctm"), "{text}");
}

/// fails if a cutover that got as far as registering is called "never
/// moved" and left alone.
///
/// `cut_over` writes `deployed` only on its fully verified path. Its
/// `NotVerified` and `Failed` arms return without touching it, having
/// already run `undo_start`, and `undo_start`'s own repair can fail and
/// leave a newcomer registered. So an absent `deployed` means the cutover
/// did not LAND, not that it never RAN, and reading it alone reported a
/// sheep in that state as running where it belongs, doing nothing, at the
/// one moment this module exists to catch exactly that.
///
/// The shepherd's own registration is what separates the two, because
/// `cut_over` is the only thing that ever points a sheep's `cwd` at the
/// tree's `current`.
#[tokio::test]
async fn a_cutover_that_registered_is_restored_even_without_a_deployed_sha() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_never_cut_over(home.path(), "ctm", "/srv/ctm", "./run.sh");
    let tree = Tree::for_sheep(home.path(), "ctm");
    let daemon = Recording::with_registered(&["ctm"]).registered_at(&tree.current());

    let results = all(&daemon, home.path()).await;

    let text = report(&results);
    assert!(
        !text.contains("never moved"),
        "a sheep the cutover registered against the tree was moved: {text}"
    );
    assert!(
        !daemon.calls().is_empty(),
        "it must actually be put back, not merely reported"
    );
}

/// fails if one target's failure stops the others being restored, or
/// stops the removal. An operator asking to remove something is
/// entitled to have it removed, and a dog that refused to be
/// uninstalled because one of five sheep would not restart would be
/// worse than one that did nothing at all.
#[tokio::test]
async fn a_failure_is_reported_and_the_rest_still_run() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "aaa", "/srv/a", "./a");
    write_target_with_origin(home.path(), "zzz", "/srv/z", "./z");
    let daemon = Recording::refusing_first_start_only(&["aaa", "zzz"]);

    let results = all(&daemon, home.path()).await;

    assert_eq!(results.len(), 2);
    // `Reset`, not `Lost`: the fallback re-registered what the shepherd
    // already had, so "aaa" is still running. A double that refused
    // EVERY start would give `Lost` here, which is a different claim and
    // has its own test below.
    assert!(
        matches!(results[0], Restored::Reset { .. }),
        "{:?}",
        results[0]
    );
    assert!(matches!(results[1], Restored::Returned { .. }));
    assert!(report(&results).contains("aaa"), "the failure is named");
}

/// fails if a refused restore leaves the sheep deleted when it did not
/// have to be. `Delete` is stop plus deregister and the roll drops a
/// name with no live instance, so the window between the delete and a
/// refused `Start` is one where the sheep is gone from both. The
/// fallback re-registers what the shepherd had a moment ago, which is
/// the right answer for the common causes: a transient refusal, a bad
/// origin_script, a `user` that no longer resolves.
#[tokio::test]
async fn a_refused_restore_puts_the_shepherds_own_config_back() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_first_start_only(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(results[0], Restored::Reset { .. }),
        "{:?}",
        results[0]
    );
    assert_eq!(daemon.started().len(), 2, "the restore, then the fallback");
    assert_eq!(
        daemon.started()[1].cwd.as_deref(),
        Some("/srv/deploy-tree/current"),
        "the fallback re-registers what the shepherd had, not the restore"
    );
}

/// fails if a rescued restore is reported with wording that claims
/// nothing happened. The delete already ran and `start(vec![current])`
/// then succeeded, so the sheep was stopped and a fresh instance
/// started - not "left as it is" in any sense an operator would
/// recognise, even though the registered config now matches what it
/// was a moment ago.
#[tokio::test]
async fn a_rescued_restore_is_worded_as_a_reset_not_left_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_first_start_only(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    let text = report(&results);
    assert!(!text.contains("left as it is"), "{text}");
    assert!(text.contains("stopped it and started it again"), "{text}");
}

/// fails if a sheep that really has been deleted is reported as merely
/// "could not be restored". Every other outcome here leaves a running
/// app; this one does not, and an operator reading the gentler wording
/// would assume theirs was still up. This is the one row in the whole
/// report that has to be alarming.
#[tokio::test]
async fn a_sheep_left_deleted_says_so_in_those_words() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_every_start(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(results[0], Restored::Lost { .. }),
        "{:?}",
        results[0]
    );
    let text = report(&results);
    assert!(text.contains("NO LONGER REGISTERED"), "{text}");
    assert!(text.contains("will not come back"), "{text}");
}

/// fails if the deploy tree is removed. It is not the dog's to delete,
/// and in the bootstrap case a running app is still pointing into it,
/// so deleting it would take down an app during an uninstall.
#[tokio::test]
async fn the_deploy_tree_is_left_on_disk() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    all(&Recording::with_registered(&["bpm"]), home.path()).await;
    assert!(home.path().join("deploy/bpm/deploy.toml").is_file());
}

/// fails if a muster roll that cannot be read produces a separate,
/// fabricated "not registered" row per pre-existing sheep instead of
/// one row naming all of them and the roll's own real cause. An
/// operator meeting N copies of a wrong guess has less to act on than
/// one line naming the actual reason - here, `roll::read`'s "newer
/// shepherd" message - and the affected sheep.
#[tokio::test]
async fn a_roll_read_failure_is_reported_once_dog_wide_with_the_real_cause() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "aaa", "/srv/a", "./a");
    write_target_with_origin(home.path(), "zzz", "/srv/z", "./z");
    let daemon = Recording::with_unreadable_roll();

    let results = all(&daemon, home.path()).await;

    assert_eq!(
        results.len(),
        1,
        "one row for the whole roll failure, not one per sheep: {results:?}"
    );
    assert!(matches!(results[0], Restored::RollUnreadable { .. }));
    let text = report(&results);
    assert!(text.contains("aaa"), "{text}");
    assert!(text.contains("zzz"), "{text}");
    assert!(text.contains("newer"), "{text}");
    assert!(
        daemon.calls().is_empty(),
        "nothing was deleted or started without a readable roll"
    );
}

/// fails if a shepherd that refuses `describe` outright is reported as
/// anything other than `Failed`, or if the refusal is silently
/// swallowed into `Done`. `put_back` cannot learn what to delete
/// without this call, so nothing about the sheep changes.
#[tokio::test]
async fn a_describe_that_is_refused_leaves_the_sheep_untouched() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_describe(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(results[0], Restored::Failed { .. }),
        "{:?}",
        results[0]
    );
    assert!(daemon.calls().is_empty(), "nothing was deleted or started");
}

/// fails if a `delete` that fails PARTWAY through a multi-instance
/// sheep is reported as anything other than its own `PartlyDeleted`, or
/// if the loop keeps deleting instances after one refuses. This is the
/// single most dangerous path in the file: some instances may already
/// be gone and others still live, and neither `Failed`'s "left as it
/// is" nor `Lost`'s "gone from the flock" is a true sentence about it -
/// only `shep describe` can say what is really there.
#[tokio::test]
async fn a_delete_that_fails_partway_through_is_reported_as_partly_deleted() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_delete_partway(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(results[0], Restored::PartlyDeleted { .. }),
        "{:?}",
        results[0]
    );
    assert_eq!(
        daemon.calls(),
        vec!["delete"],
        "the first delete lands before the second refuses, and nothing after it runs"
    );
    assert!(
        daemon.started().is_empty(),
        "start is never reached once a delete fails"
    );
}

/// fails if a partly-deleted sheep is worded as though it were either
/// fully running or fully removed. Neither `Failed`'s "left as it is"
/// nor `Lost`'s "gone from the flock" is true here, and this pins the
/// two true things the report can say: check `shep describe` for what
/// is actually there, and `shep flock` for whether it is still
/// registered - this crate cannot know that from here, since `Delete`
/// landing on the LAST remaining instance really does deregister it.
#[tokio::test]
async fn a_partly_deleted_sheep_is_worded_as_neither_running_nor_removed() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_delete_partway(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    let text = report(&results);
    assert!(!text.contains("left as it is"), "{text}");
    assert!(!text.contains("gone from the flock"), "{text}");
    assert!(!text.contains("never touched"), "{text}");
    assert!(
        text.contains("neither fully running nor fully removed"),
        "{text}"
    );
    assert!(text.contains("shep describe bpm"), "{text}");
    assert!(text.contains("shep flock"), "{text}");
}

/// fails if a delete refused on the FIRST instance, with nothing behind
/// it, is reported as `PartlyDeleted`. Nothing landed: the sheep is
/// fully running and fully registered, which is exactly `Failed`'s
/// "left as it is" case. The discriminator for `PartlyDeleted` is
/// whether a delete already landed, not whether one failed - a
/// single-instance sheep whose only delete is refused must stay
/// `Failed`, and this is the ordinary shape a refused delete takes.
#[tokio::test]
async fn a_delete_refused_with_nothing_landed_yet_stays_failed() {
    let home = tempfile::tempdir().expect("tempdir");
    write_target_with_origin(home.path(), "bpm", "/srv/reactmap", "bun .");
    let daemon = Recording::refusing_first_delete_only(&["bpm"]);

    let results = all(&daemon, home.path()).await;

    assert!(
        matches!(results[0], Restored::Failed { .. }),
        "{:?}",
        results[0]
    );
    let text = report(&results);
    assert!(text.contains("nothing was changed by this dog"), "{text}");
    assert!(
        !text.contains("neither fully running nor fully removed"),
        "{text}"
    );
}

/// fails if `on-remove` ever turns a partial failure into a nonzero
/// exit. There is no such branch in `main.rs`'s `on_remove` - this pins
/// the half of that contract that lives here: a report containing a
/// `Failed` row still names every other row plainly, so nothing about
/// this module's own output would justify one.
#[test]
fn a_report_with_a_failure_still_names_every_other_row() {
    let results = vec![
        Restored::Failed {
            sheep: "aaa".to_owned(),
            why: "refused".to_owned(),
        },
        Restored::Returned {
            sheep: "zzz".to_owned(),
            to: PathBuf::from("/srv/z"),
        },
    ];
    let text = report(&results);
    assert!(text.contains("aaa"), "{text}");
    assert!(text.contains("zzz"), "{text}");
}

/// fails if a deploy directory that exists but cannot be listed is
/// swallowed into an empty report. `paths::targets` returns that error
/// rather than treating it as "nothing here", and dropping it left
/// `report(&[])` printing nothing and `on_remove` exiting 0 with every
/// sheep under the tree left running.
#[tokio::test]
async fn a_deploy_directory_that_cannot_be_listed_is_reported_not_silenced() {
    let home = tempfile::tempdir().expect("tempdir");
    // A file where a directory is expected makes `read_dir` fail with
    // something other than `NotFound`, which `paths::targets` does not
    // treat as "nothing to restore".
    fs::write(home.path().join("deploy"), b"not a directory").expect("write file");

    let results = all(&Recording::new(&[], Refuse::Never), home.path()).await;

    assert_eq!(
        results.len(),
        1,
        "the failure must be reported, not swallowed: {results:?}"
    );
    assert!(
        matches!(&results[0], Restored::Unlisted { dir, .. } if dir.ends_with("deploy")),
        "got: {:?}",
        results[0]
    );
    assert!(
        !report(&results).is_empty(),
        "silence here reads as success"
    );
}

/// fails if a run with no deploy targets at all prints nothing. Silence
/// is indistinguishable from success, and `main.rs`'s sibling branch
/// prints a sentence for exactly this reason.
#[test]
fn an_empty_report_says_there_was_nothing_to_restore() {
    assert_eq!(report(&[]), "no deploy targets, nothing to restore\n");
}

/// fails if the dog and the daemon spell `$SHEP_HOME` differently and a
/// sheep the cutover DID register reads as never moved because of it.
/// `daemon.rs:141` says the two can resolve the same directory through
/// different paths, so the comparison has to see through that rather
/// than compare spellings literally.
#[tokio::test]
async fn a_registration_reached_through_a_symlinked_parent_is_recognised() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&home, &link).expect("symlink parent");

    let tree = Tree::for_sheep(&home, "ctm");
    fs::create_dir_all(tree.root()).expect("create tree");
    let release = tmp.path().join("release");
    fs::create_dir_all(&release).expect("create release dir");
    std::os::unix::fs::symlink(&release, tree.current()).expect("symlink current");

    write_target_never_cut_over(&home, "ctm", "/srv/ctm", "./run.sh");
    let cwd_through_link = link.join("deploy").join("ctm").join("current");
    let daemon = Recording::with_registered(&["ctm"]).registered_at(&cwd_through_link);

    let results = all(&daemon, &home).await;

    let text = report(&results);
    assert!(
        !text.contains("never moved"),
        "the two spellings resolve to the same directory: {text}"
    );
    assert!(
        !daemon.calls().is_empty(),
        "it must actually be put back, not merely reported"
    );
}

/// fails if the same recognition breaks once the release `current`
/// pointed at is gone. Both full canonicalisations fail on a dangling
/// link, and a fallback to the literal spellings called the two names
/// for one `current` different, so the sheep read as never moved and
/// stayed registered against the tree.
#[tokio::test]
async fn a_registration_through_a_symlinked_parent_is_recognised_when_current_dangles() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&home, &link).expect("symlink parent");

    let tree = Tree::for_sheep(&home, "ctm");
    fs::create_dir_all(tree.root()).expect("create tree");
    std::os::unix::fs::symlink(tree.release("gone"), tree.current()).expect("dangling current");

    write_target_never_cut_over(&home, "ctm", "/srv/ctm", "./run.sh");
    let cwd_through_link = link.join("deploy").join("ctm").join("current");
    let daemon = Recording::with_registered(&["ctm"]).registered_at(&cwd_through_link);

    let results = all(&daemon, &home).await;

    let text = report(&results);
    assert!(!text.contains("never moved"), "{text}");
    assert!(!daemon.calls().is_empty(), "put back, not merely reported");
}
