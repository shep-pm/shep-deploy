use super::*;
use crate::fixtures;
use crate::optin::test_support::{
    RollBuilding, RollOf, SANDBOXED, checkout_with_commit, write_target,
};

/// The directory `sha`'s release lives in for the remote
/// [`fixtures::state`] names, which is the one every [`write_target`] record
/// here carries.
fn release_of(tree: &Tree, sha: &str) -> std::path::PathBuf {
    tree.release(
        &fixtures::state().remote,
        sha,
        &fixtures::default_build_key(&fixtures::state().checkout),
    )
}

/// fails if a sheep that is already a target can be opted in again.
/// The second run would `git init --bare` over a live tree and rebuild
/// a release for an app whose sheep is already running from `current`,
/// which is a way to break a working deployment with a command whose
/// name sounds safe.
#[tokio::test]
async fn opting_in_twice_is_refused_by_name() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    write_target(
        home.path(),
        "bpm",
        Some("0123456789abcdef0123456789abcdef01234567"),
    );
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("refuses");
    let shown = err.to_string();
    assert!(shown.contains("bpm"), "{shown}");
    assert!(shown.contains("already"), "{shown}");
}

/// fails if opt-in stops recording how the sheep ran BEFORE it took
/// over. These two fields are the only record there is, and losing them
/// means removing the dog leaves the app running from a path under
/// $SHEP_HOME the operator has no reason to know about, which is the
/// exact failure the restore exists to prevent.
#[tokio::test]
async fn the_pre_adoption_cwd_and_script_are_recorded() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    assert_eq!(prepared.state.origin_cwd.as_deref(), Some(checkout.path()));
    assert_eq!(prepared.state.origin_script.as_deref(), Some("./run.sh"));
    assert_eq!(prepared.state.checkout, checkout.path());
}

/// fails if the branch stops coming from the operator's own checkout.
/// It is read there once, at setup, so `git checkout stable` before
/// `setup` picks the branch and nobody learns a new config key. After
/// that `--branch` changes it, and the checkout is not read again.
#[tokio::test]
async fn the_branch_comes_from_the_checkouts_own_head() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    fixtures::run_git(checkout.path(), &["checkout", "-q", "-b", "stable"]);
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");
    assert_eq!(prepared.state.branch, "stable");
    assert_eq!(daemon.settings().branch.as_deref(), Some("stable"));
}

/// fails if setup leaves the settings out of the sheep's table, or
/// writes them to the record. The table is where lookout shows them,
/// and `manual` is what keeps the loop off a tree nothing has served
/// from yet.
#[tokio::test]
async fn prepare_writes_the_settings_to_the_table_and_not_the_record() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    let held = daemon.settings();
    assert_eq!(held.branch.as_deref(), Some("main"));
    assert_eq!(held.watch, Watch::Manual);
    assert_eq!(held.verify, Verify::Probed);
    let on_disk: toml::Table = std::fs::read_to_string(prepared.tree.state_file())
        .expect("reads")
        .parse()
        .expect("a table");
    for key in ["branch", "watch", "verify"] {
        assert!(
            !on_disk.contains_key(key),
            "{key} in the record: {on_disk:?}"
        );
    }
}

/// fails if a run that died before writing its record cannot simply be
/// run again, which is what this function's own doc promises twice.
///
/// The promise was false from `worktree_add` onward. `prepare` had no
/// existence guard and no cleanup on any failure path, so the release
/// directory a dead run left behind made the next run fail with a raw
/// `fatal: '<path>' already exists` from git. Verified 2026-08-28.
///
/// Modelled here at the narrowest window the doc calls out by name, a
/// kill between `swap::point_at` and `state.write`: the release and
/// `current` exist, `deploy.toml` does not, so the "already a deploy
/// target" refusal at the top correctly does not fire and the run gets
/// as far as the checkout before failing.
#[tokio::test]
async fn a_prepare_that_died_before_writing_its_record_runs_again() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let first = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");
    let (first_sha, state_file) = (first.sha.clone(), first.tree.state_file());
    // The run died, so its hold on the tree died with it: `Prepared`
    // carries the lock, and a second run against a held tree is refused
    // for the reason `crate::lock` gives, which is not the case here.
    drop(first);
    std::fs::remove_file(&state_file).expect("the record a kill never wrote");

    let again = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("a tree with no record must be resumable, as the doc says");
    assert_eq!(again.sha, first_sha);
    assert!(
        state_file.is_file(),
        "the second run must leave the record the first one never wrote"
    );
}

/// fails if `current` does not end up pointing at a real release
/// holding the shared files. This is the whole deliverable of part one:
/// a tree a cutover can point a sheep at.
#[tokio::test]
async fn current_ends_up_on_a_release_carrying_the_shared_files() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    std::fs::write(checkout.path().join(".gitignore"), "local.json\n").expect("write");
    std::fs::write(checkout.path().join("local.json"), "{}").expect("write");
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    let current = swap::resolve(&prepared.tree.current())
        .expect("reads")
        .expect("current is set");
    assert_eq!(
        current,
        prepared.tree.release(
            &prepared.state.remote,
            &prepared.sha,
            &fixtures::default_build_key(&prepared.state.checkout),
        ),
    );
    assert_eq!(
        std::fs::read_to_string(current.join("local.json")).expect("reads through the link"),
        "{}"
    );
}

/// fails if a checkout on a detached HEAD is accepted. There is no
/// branch to track, so there is nothing to poll, and a target created
/// this way would silently never deploy again.
#[tokio::test]
async fn a_detached_checkout_is_refused() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let head = fixtures::head_of(checkout.path());
    fixtures::run_git(checkout.path(), &["checkout", "-q", &head]);
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("refuses");
    assert!(err.to_string().contains("detached"), "{err}");
}

/// fails if the record stops carrying the app as the shepherd had it
/// before adoption. Removal restores from this; `cwd` and `script` alone
/// left the deployed repository's `env`, instances and probes in place.
#[tokio::test]
async fn the_record_carries_the_whole_pre_adoption_definition() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect("prepares");

    let origin = prepared.state.origin.as_ref().expect("recorded");
    assert_eq!(origin, &prepared.previous_config);
    let on_disk = State::read(&prepared.tree.state_file()).expect("reads");
    assert_eq!(on_disk.origin.as_ref(), Some(&prepared.previous_config));
}

/// fails if a sheep registered with a `sandbox` key is taken over. The
/// key is never serialized by shep-core 0.23, so the cutover's `Start`
/// would register the sheep without it, outside crabpot, with nothing
/// said. A 0.21 or 0.22 shepherd still sends the key in its roll.
#[tokio::test]
async fn a_sheep_with_a_sandbox_key_is_refused_before_anything_is_written() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries).with_fields(SANDBOXED);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("refuses");

    assert!(err.to_string().contains("sandbox"), "{err}");
    assert!(
        !Tree::for_sheep(home.path(), "bpm").root().exists(),
        "a refused init leaves no tree behind"
    );
}

/// fails if a sheep the shepherd already runs from `current` is treated
/// as one to take over. That is what a cutover that landed and could not
/// write its record leaves, and the old refusal told the operator to
/// remove the tree the service runs from. The record is repaired from
/// what `current` names and the sheep is refused as the target it is.
#[tokio::test]
async fn a_sheep_already_running_from_current_has_its_record_repaired() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "bpm");
    std::fs::create_dir_all(tree.root()).expect("a tree");
    let release = release_of(&tree, fixtures::SHA);
    std::fs::create_dir_all(&release).expect("a release directory");
    swap::point_at(&tree.current(), &release).expect("current");
    write_target(home.path(), "bpm", None);
    let current = tree.current();
    let entries = [("bpm", current.as_path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("already a target");

    let shown = err.to_string();
    assert!(shown.contains("already a deploy target"), "{shown}");
    assert!(shown.contains("brought up to date"), "{shown}");
    assert!(
        !shown.contains("Remove"),
        "must never say to remove the tree: {shown}"
    );
    assert_eq!(
        State::read(&tree.state_file())
            .expect("reads")
            .deployed
            .as_deref(),
        Some(fixtures::SHA),
        "the record now names what current names"
    );
}

/// fails if a second spelling of the tree's `current` is not recognised
/// as the tree's `current`. The daemon hands back the cwd as it was
/// registered, and it can spell `$SHEP_HOME` through a symlink the dog
/// does not use. A literal comparison then skipped this branch, found
/// the record with no `deployed`, and told the operator to remove the
/// tree a running service was serving from.
#[tokio::test]
async fn a_sheep_running_from_current_spelled_through_a_link_is_still_recognised() {
    let root = tempfile::tempdir().expect("tempdir");
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&home, &link).expect("a second spelling");
    let tree = Tree::for_sheep(&home, "bpm");
    std::fs::create_dir_all(tree.root()).expect("a tree");
    let release = release_of(&tree, fixtures::SHA);
    std::fs::create_dir_all(&release).expect("a release directory");
    swap::point_at(&tree.current(), &release).expect("current");
    write_target(&home, "bpm", None);
    let spelled_through_link = link.join("deploy/bpm/current");
    let entries = [("bpm", spelled_through_link.as_path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, &home, "bpm", &fixtures::dog_config())
        .await
        .expect_err("already a target");

    let shown = err.to_string();
    assert!(shown.contains("already a deploy target"), "{shown}");
    assert!(
        !shown.contains("Remove"),
        "must never say to remove the tree: {shown}"
    );
}

/// fails if the same sheep with NO record at all is told anything but
/// that the record is missing. Nothing can rebuild the origin fields, so
/// the operator writes the file; the tree still must not be removed.
#[tokio::test]
async fn a_sheep_running_from_current_with_no_record_is_told_to_write_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "bpm");
    std::fs::create_dir_all(tree.root()).expect("a tree");
    let release = release_of(&tree, fixtures::SHA);
    std::fs::create_dir_all(&release).expect("a release directory");
    swap::point_at(&tree.current(), &release).expect("current");
    let current = tree.current();
    let entries = [("bpm", current.as_path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("already a target");

    let shown = err.to_string();
    assert!(shown.contains("record is missing"), "{shown}");
    assert!(!shown.contains("Remove"), "{shown}");
}

/// fails if a record that cannot be READ is treated as evidence of
/// anything. The two branches below give opposite advice and one of
/// them says to remove the tree, so a guess has no cautious side: a
/// live, cut-over sheep's working directory is `<tree>/current`, and an
/// operator who follows that instruction on one deletes a running
/// service's cwd. Permissions, a hand-edit typo and corruption all
/// reach here, and none of them is an abandoned cutover.
///
/// It is the only irreversible thing this crate can tell somebody to
/// do, which is why it refuses rather than picking the branch that
/// looked safer in the direction it was thought about.
#[tokio::test]
async fn a_record_that_cannot_be_read_refuses_rather_than_guessing() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    write_target(
        home.path(),
        "bpm",
        Some("0123456789abcdef0123456789abcdef01234567"),
    );
    let tree = Tree::for_sheep(home.path(), "bpm");
    std::fs::write(tree.state_file(), "this is not toml = = =").expect("corrupt the record");
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("refuses");

    let shown = err.to_string();
    assert!(shown.contains("cannot be read"), "{shown}");
    assert!(
        shown.contains("Do NOT remove the tree"),
        "the irreversible instruction is withheld: {shown}"
    );
    assert!(
        !shown.contains("was never cut over"),
        "it must not claim the cutover was abandoned: {shown}"
    );
}

/// fails if a target whose cutover was ABANDONED is pointed at
/// `shep deploy`. Both situations trip the same `deploy.toml` check,
/// and they need opposite advice. A target that was cut over serves
/// from its tree, so `shep deploy` is exactly right. A target whose
/// cutover was abandoned has served nothing from it, and its record
/// names no release, so `deploy` does not short-circuit: it builds,
/// points `current` at the same release, and reloads the sheep BY NAME
/// at the operator's own checkout. shep replaces that instance from its
/// own spec, `verify::wait` sees a full pid turnover, and the deploy
/// prints success for a release nothing ever served. That is the exact
/// class the engine plan spent five rounds removing, reachable through
/// one sentence of advice.
#[tokio::test]
async fn an_abandoned_target_is_not_pointed_at_shep_deploy() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    write_target(home.path(), "bpm", None);
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let err = prepare(&daemon, home.path(), "bpm", &fixtures::dog_config())
        .await
        .expect_err("refuses");

    let shown = err.to_string();
    assert!(shown.contains("never cut over"), "{shown}");
    assert!(shown.contains("Do NOT run `shep deploy bpm`"), "{shown}");
    assert!(
        shown.contains("shep-deploy init bpm"),
        "it says how to try again: {shown}"
    );
}

/// fails if setup takes over a sheep its first deploy would refuse, or
/// pays for a build before it says so. The refusal at the first deploy
/// lands after the cutover, with the sheep already running from the tree.
#[tokio::test]
async fn an_ungated_app_under_probed_is_refused_before_it_is_built_or_recorded() {
    let home = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("tempdir");
    let marker = scratch.path().join("built");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollBuilding::new(&entries, &marker).ungated();

    let err = prepare_with(
        &daemon,
        home.path(),
        "bpm",
        &fixtures::dog_config(),
        Verify::Probed,
    )
    .await
    .expect_err("refuses");

    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(
        err.to_string().contains("--verify alive"),
        "it names the way out: {err}"
    );
    assert!(
        !err.to_string().contains("deploy.toml"),
        "no record exists to edit: {err}"
    );
    let tree = Tree::for_sheep(home.path(), "bpm");
    assert!(!marker.exists(), "the build ran");
    assert!(!tree.state_file().exists(), "a record was written");
    assert_eq!(swap::resolve(&tree.current()).expect("reads"), None);
}

/// fails if a refused setup leaves anything a second one trips on. The
/// operator's next move after the refusal is the same command with
/// `--verify alive`, so the tree the first run touched must not count as
/// a target.
#[tokio::test]
async fn a_refused_setup_can_be_retried_with_alive() {
    let home = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let built = scratch.path().join("built");
    let daemon = RollBuilding::new(&entries, &built).ungated();
    let config = fixtures::dog_config();
    prepare_with(&daemon, home.path(), "bpm", &config, Verify::Probed)
        .await
        .expect_err("refuses");

    let again = prepare_with(&daemon, home.path(), "bpm", &config, Verify::Alive).await;

    assert!(again.is_ok(), "{:?}", again.err());
}

/// fails if `--verify alive` is not what the record says. The forward
/// deploy reads `verify` from this record, and an `alive` that setup
/// accepted but wrote as `probed` strands the app on its first push.
#[tokio::test]
async fn an_ungated_app_under_alive_is_prepared_and_recorded_as_alive() {
    let home = tempfile::tempdir().expect("tempdir");
    let scratch = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let built = scratch.path().join("built");
    let daemon = RollBuilding::new(&entries, &built).ungated();

    let prepared = prepare_with(
        &daemon,
        home.path(),
        "bpm",
        &fixtures::dog_config(),
        Verify::Alive,
    )
    .await
    .expect("prepares");

    assert_eq!(prepared.state.verify, Verify::Alive);
    assert_eq!(daemon.settings().verify, Verify::Alive);
}

/// fails if the check refuses an app that does have a gate, which would
/// make `--verify alive` mandatory for every sheep.
#[tokio::test]
async fn an_app_with_wait_ready_is_prepared_under_probed() {
    let home = tempfile::tempdir().expect("tempdir");
    let checkout = checkout_with_commit();
    let entries = [("bpm", checkout.path())];
    let daemon = RollOf::new(&entries);

    let prepared = prepare_with(
        &daemon,
        home.path(),
        "bpm",
        &fixtures::dog_config(),
        Verify::Probed,
    )
    .await
    .expect("prepares");

    assert_eq!(prepared.state.verify, Verify::Probed);
}
