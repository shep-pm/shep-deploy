//! Part one of opt-in: the deploy tree and its first release.

use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::build;
use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::refuse_ungated_at_setup;
use crate::error::Error;
use crate::git;
use crate::lock;
use crate::paths::{self, Repo, Tree};
use crate::roll;
use crate::settings::Change;
use crate::shared;
use crate::state::{State, Verify, Watch};
use crate::swap;

/// Everything opt-in built, ready for a cutover to register and swap into
/// place.
///
/// Carries the tree's exclusive hold as well, so the lock [`prepare`] took
/// is still held while [`super::cut_over`] registers, deletes and writes the
/// record. Released between the two, a second `setup` in that window read a
/// record with no `deployed` and told the operator to remove a tree a live
/// cutover was using.
#[derive(Debug)]
pub struct Prepared {
    /// The tree's exclusive hold, kept for the cutover. See the type doc.
    hold: lock::Deploying,
    /// The deploy tree this release was built into.
    pub tree: Tree,
    /// The tree's own record, with `current` already pointed at `sha`.
    pub state: State,
    /// The sha `tree.current()` resolves to.
    pub sha: String,
    /// The release's app definition, which [`super::cut_over`] registers after
    /// overriding `cwd`.
    pub app: AppConfig,
    /// The app definition as the shepherd has it RIGHT NOW, carried so an
    /// abandoned cutover can put the shepherd's persisted record back.
    /// See [`super::undo::undo_start`]: an accepted `Start` records against the sheep's
    /// name, and deleting the instance does not undo that.
    pub previous_config: AppConfig,
}

impl Prepared {
    /// Splits the tree's hold from the rest, so [`super::cut_over`] can keep
    /// it to the end while it works with the fields.
    ///
    /// The hold has no other way out: dropping it is what releases the lock.
    pub(super) fn into_parts(self) -> (lock::Deploying, Tree, State, String, AppConfig, AppConfig) {
        (
            self.hold,
            self.tree,
            self.state,
            self.sha,
            self.app,
            self.previous_config,
        )
    }
}

/// Builds `sheep`'s deploy tree and its first release, from the checkout
/// shep currently has it registered against.
///
/// Refuses outright, before touching anything, if `sheep` is already a
/// target - see the module doc for why a re-run has to be refused rather
/// than treated as a retry. Everything after that refusal either succeeds
/// all the way through to a release under `current`, or fails partway and
/// leaves a directory behind with no `deploy.toml` in it, which is not a
/// target and does not trip that same refusal on a second attempt.
///
/// The sheep's table gets `watch = "manual"`, whatever the sheep ends
/// up with: nothing has been served from the tree yet, and [`super::cut_over`] is
/// what promotes it. A cutover abandoned partway therefore leaves a target
/// the poll loop passes over rather than one it refuses every interval.
///
/// # Errors
/// [`Error::Config`] if `sheep` is already a deploy target, if the
/// shepherd has no sheep by that name registered, or if it records no
/// working directory for it. Otherwise, whatever [`roll::registered`],
/// [`git::remote_url`], [`git::current_branch`], [`git::init_bare`],
/// [`git::fetch`], [`git::remote_head`], [`git::worktree_add`],
/// [`crate::deploy::stage_release`] (the worktree, the cache link, the shared
/// files),
/// [`build::run`] or [`swap::point_at`] return.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "setup calls prepare_with; tests keep the default")
)]
pub async fn prepare<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    sheep: &str,
    config: &DogConfig,
) -> Result<Prepared, Error> {
    prepare_with(daemon, shep_home, sheep, config, Verify::default()).await
}

/// As [`prepare`], recording `verify` as the target's mode instead of the
/// default.
///
/// The first deploy refuses a `probed` target whose app declares no readiness
/// gate, after the cutover has already landed. That refusal is made here
/// instead, once the release is staged and before it is built, so
/// the run leaves no record and no swap and a second `setup` is a retry. A
/// sheep with nothing to probe is taken over with [`Verify::Alive`].
///
/// # Errors
/// As [`prepare`], and [`Error::Config`] if `verify` is [`Verify::Probed`] and
/// the release's app has neither a `readiness_probe` nor `wait_ready`.
pub async fn prepare_with<D: Daemon>(
    daemon: &D,
    shep_home: &Path,
    sheep: &str,
    config: &DogConfig,
    verify: Verify,
) -> Result<Prepared, Error> {
    if sheep == paths::REPOS {
        return Err(Error::Config(format!(
            "{sheep:?} is where the clones shared between sheep live, so no sheep can be called \
             that: rename it in its Flockfile and run setup again"
        )));
    }
    let tree = Tree::for_sheep(shep_home, sheep);

    // Cloned rather than borrowed, and named `previous_config` rather than
    // `app`, because the release's own definition shadows the name below and
    // these two must never be confused: this one is the operator's, and it
    // is the only copy of it that exists once Task 8 sends its Start.
    let registered = roll::registered(daemon).await?;
    let previous_config = registered.get(sheep).cloned().ok_or_else(|| {
        Error::Config(format!(
            "the shepherd has no sheep named {sheep:?} registered, so there is nothing to take \
             over: `shep-deploy survey` lists every registered sheep and where it stands"
        ))
    })?;
    let checkout = PathBuf::from(previous_config.cwd.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "shep records no working directory for {sheep}, so there is no checkout to deploy \
             from"
        ))
    })?);
    // The shepherd already runs the sheep from this tree's `current`: a
    // cutover landed and its record was not written, or the record was lost
    // afterwards. Either way the tree is live, and the advice the record
    // branch below gives an unrecorded tree, remove it and run setup again,
    // would take a running service's cwd with it. The record is repaired
    // from what `current` names instead, and the sheep is then what it is:
    // a deploy target, refused here like any other.
    if shared::same_path(&checkout, &tree.current())
        && let Some(release) = swap::resolve(&tree.current())?
        && let Some(sha) = release.file_name().and_then(|name| name.to_str())
        && crate::state::is_sha(sha)
        && release.is_dir()
    {
        // The tree exists in this case, so the hold litters nothing, and the
        // repair below writes the record under it.
        let _deploying = lock::hold(&tree)?;
        let _record = lock::hold_record(&tree)?;
        let next = format!(
            "Deploy it with `shep deploy {sheep}`, or change how it is watched with `shep \
             deploy {sheep} --watch auto|manual`."
        );
        let record = match State::read(&tree.state_file()) {
            Ok(mut state @ State { deployed: None, .. }) => {
                state.deployed = Some(sha.to_owned());
                state.write(&tree.state_file())?;
                format!(
                    "Its record named no deployed release and has been brought up to date \
                     with that. {next}"
                )
            }
            Ok(state) if state.deployed.as_deref() == Some(sha) => {
                format!("Its record agrees. {next}")
            }
            Ok(State {
                deployed: Some(other),
                ..
            }) => format!(
                "Its record names {} instead, which is what a deploy whose record could not \
                 be written leaves; the next deploy corrects it. {next}",
                &other[..7.min(other.len())]
            ),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                "Its record is missing, and this dog cannot rebuild the origin fields removal \
                 needs: write a deploy.toml by hand from another target's as a model, with \
                 `deployed` set to that sha, before deploying."
                    .to_owned()
            }
            Err(_) => "Its record cannot be read: repair it before deploying, and do NOT \
                       remove the tree, the service runs from inside it."
                .to_owned(),
        };
        return Err(Error::Config(format!(
            "{sheep} is already a deploy target: the shepherd runs it from {}, which names {}. \
             {record}",
            tree.current().display(),
            &sha[..7]
        )));
    }

    // A cwd anywhere else inside this sheep's own deploy tree is what an
    // abandoned cutover leaves in the shepherd's record once the tree's own
    // record is gone, and capturing it as the origin would make removal put
    // the sheep back INTO the tree. Refused by name rather than left to
    // `current_branch`, which happened to refuse it only because releases
    // are detached worktrees.
    if shared::resolved(&checkout).starts_with(shared::resolved(tree.root())) {
        return Err(Error::Config(format!(
            "{sheep} is registered with its working directory inside its own deploy tree ({}), \
             so there is no operator checkout to take it over from. That is what an abandoned \
             cutover leaves in the shepherd's record: re-register {sheep} from its own \
             Flockfile in the checkout it should deploy from, then run setup again",
            checkout.display()
        )));
    }

    if tree.state_file().is_file() {
        // Two very different situations share this one condition, and
        // telling them apart is not a nicety. A target that was cut over
        // has served from its tree and `shep deploy` is exactly right for
        // it. A target whose cutover was ABANDONED has served nothing, and
        // `shep deploy` on it builds, swaps and reloads the sheep at its
        // own checkout, sees a full pid turnover because the reload really
        // did replace that instance, and reports success for a release
        // nothing ever ran. `deployed` is what separates them.
        //
        // A record that cannot be READ is neither, and guessing has no
        // cautious side: the two branches give opposite advice and one of
        // them says to remove the tree. A cut-over sheep's cwd is inside
        // that tree, so following it on a live target destroys a running
        // service's working directory, which is the one thing here that
        // cannot be undone. So this refuses instead.
        let state = State::read(&tree.state_file()).map_err(|source| {
            Error::Config(format!(
                "{sheep} has a deploy tree at {} and its record at {} cannot be read. \
                 Refusing rather than guessing. That record is the only thing that says \
                 whether {sheep} was ever cut over, and the two answers need opposite \
                 handling. Do NOT remove the tree until you know which it is: if {sheep} WAS \
                 cut over, its working directory is inside it and removing it takes a running \
                 service's cwd with it. Restore or repair that file first - `shep describe \
                 {sheep}` shows whether the sheep is running from inside this tree. Reading it \
                 failed with: {source}",
                tree.root().display(),
                tree.state_file().display()
            ))
        })?;
        return Err(Error::Config(if state.deployed.is_some() {
            format!(
                "{sheep} is already a deploy target: its tree is at {}. Deploy it with \
                 `shep deploy {sheep}`, or change how it is watched with \
                 `shep deploy {sheep} --watch auto|manual`.",
                tree.releases().display()
            )
        } else {
            format!(
                "{sheep} has a deploy tree at {} but was never cut over to it: its record names \
                 no deployed release, so nothing has ever been served from that tree. An \
                 abandoned first cutover leaves exactly this. Do NOT run `shep deploy {sheep}` \
                 against it - that would reload the sheep at its own checkout and report success \
                 for a release nothing served. Remove {} and run `shep-deploy setup {sheep}` \
                 again once the cause of the first failure is fixed.",
                tree.releases().display(),
                tree.root().display()
            )
        }));
    }

    // Both of these refuse rather than guess: a checkout with no `origin`
    // gets git's own complaint, and a detached HEAD is refused by name,
    // because there is no branch to track and a target created that way
    // would silently never deploy again.
    let remote = git::remote_url(&checkout)?;
    let branch = git::current_branch(&checkout)?;

    let state = State {
        remote,
        branch,
        deployed: None,
        failed: None,
        verify,
        // `manual` until the cutover lands, and `cut_over` is what promotes
        // it. A tree nothing has served from is not a deploy target - the
        // deploy path refuses one outright - so writing `auto` here would
        // hand the poll loop a target it can only refuse, once every
        // interval, for as long as the abandoned tree sits there.
        watch: Watch::Manual,
        // The two fields the whole restore depends on, captured once, here,
        // and never touched again.
        origin_cwd: Some(checkout.clone()),
        origin_script: Some(previous_config.script.clone()),
        checkout,
        // The whole definition, so removal puts back `env`, `instances`,
        // the probes and everything else the operator had, not the two
        // fields above alone.
        origin: Some(previous_config.clone()),
        staged: None,
    };
    // Checked here, before the fetch, the worktree and the build, rather
    // than found out by the record write at the very end. Neither value was
    // typed by the operator: `remote` is what `git remote get-url origin`
    // answered in the checkout and `checkout` is the cwd shep registered.
    let unrecordable = |err: Error| {
        Error::Config(format!(
            "{sheep} cannot be recorded as it is registered ({err}). `remote` is the \
                 checkout's `origin`, `branch` is the checkout's HEAD, and `checkout` is the cwd \
                 shep has for {sheep}; fix whichever is named and run setup again"
        ))
    };
    state
        .validate(Path::new("deploy.toml"))
        .map_err(unrecordable)?;
    if let Some(why) = crate::state::branch_refusal(&state.branch) {
        return Err(unrecordable(Error::Config(format!("`branch` {why}"))));
    }

    // The registered config already holds the table, so setup asks the
    // shepherd for nothing more. Before the lock, so a typo costs no tree.
    let settings = crate::settings::from_app(sheep, &previous_config)?;

    // Same hold a deploy takes, for the same reason: everything below writes
    // to this tree, and a poll tick can be doing the same at the same moment.
    // After every refusal that needs no tree, so a `setup` that fails on a
    // detached HEAD or a missing `origin` leaves nothing behind: `hold` makes
    // the tree's root itself. Carried out in `Prepared` so the cutover runs
    // under it too.
    let hold = lock::hold(&tree)?;

    std::fs::create_dir_all(tree.releases()).map_err(Error::at(tree.releases()))?;
    share_clone(&tree, shep_home, &state.remote)?;
    // Off the runtime's thread, like every git call: see `shared::off_thread`.
    let (git_dir, remote, tracked, budget) = (
        tree.git(),
        state.remote.clone(),
        state.branch.clone(),
        config.git_timeout,
    );
    shared::off_thread(move || git::fetch(&git_dir, &remote, &tracked, budget)).await?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

    let release = tree.release(&sha);
    // Shared with `deploy::attempt`, and that is what makes this function's
    // own retry story true: `stage_release` goes through `checkout_release`,
    // which reuses a finished checkout and replaces a partial one. `git
    // worktree add` alone refuses a path that already exists ("fatal:
    // `<path>` already exists") and refuses one it still has registered
    // after the directory was removed ("missing but already registered
    // worktree"), so every run that died anywhere from here onward left a
    // release directory that made the next run fail on git rather than
    // resume, which is the opposite of what the doc above promises.
    // Setup runs no hook: a cutover must not touch the app's database.
    let crate::deploy::Staged {
        app, spec, linked, ..
    } = {
        let (tree, checkout, sha) = (tree.clone(), state.checkout.clone(), sha.clone());
        let (app, settings) = (previous_config.clone(), settings.clone());
        shared::off_thread(move || {
            crate::deploy::stage_release(&tree, &checkout, &sha, &app, &settings)
        })
        .await?
    };
    // Before the build, so a refusal costs no build.
    refuse_ungated_at_setup(sheep, &app, verify)?;
    let snapshot = crate::linkcheck::Snapshot::before(sheep, &release, &linked);
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

    // Before the swap, so a failure here leaves nothing but a table the next
    // run writes again. `watch` is `manual` for the reason given where the
    // record is built, and the cutover promotes it.
    crate::settings::change(
        daemon,
        sheep,
        [
            Change::Branch(state.branch.clone()),
            Change::Watch(state.watch),
            Change::Verify(state.verify),
        ],
    )
    .await?;

    // `current` now points through a real release carrying the operator's
    // shared files - the whole deliverable of part one. `state.write` comes
    // AFTER the swap, not before: a run that dies between them leaves
    // `current` set but no `deploy.toml`, so `tree.state_file().is_file()`
    // at the top of this function still reads false and the operator can
    // simply run the command again, rather than tripping the "already a
    // deploy target" refusal on a tree that never finished becoming one.
    // Do not move this above the swap: a process killed between the two
    // statements is not something a single-threaded test can arrange, so
    // this ordering rests on this comment and on review, the same way
    // `swap::point_at`'s own atomicity claim rests on its doc rather than
    // on a test.
    swap::point_at(&tree.current(), &release)?;
    // Under the record lock like every other write of the record, though
    // nothing can race a record that does not exist yet: one rule is easier
    // to keep true than one rule with an exception.
    lock::hold_record(&tree).and_then(|_record| state.write(&tree.state_file()))?;

    Ok(Prepared {
        hold,
        tree,
        state,
        sha,
        app,
        previous_config,
    })
}

/// Gives `tree` the bare clone for `remote`: links it to the shared one,
/// making that first if no sheep with this remote has. A tree that already
/// has a clone of its own, from a setup that died before shared clones
/// existed, keeps it.
///
/// Under the clone's lock, so a removal that is sweeping the shared clone
/// for links cannot take it between its creation and this link.
///
/// # Errors
/// [`Error::Io`] or [`Error::Git`] as [`git::init_bare`] and
/// [`Tree::link_clone`] return them, and [`Error::Io`] if the lock cannot be
/// taken.
fn share_clone(tree: &Tree, shep_home: &Path, remote: &str) -> Result<(), Error> {
    if tree.owns_clone() {
        return git::init_bare(&tree.git());
    }
    let repo = Repo::for_remote(shep_home, remote);
    let _cloning = lock::hold_clone(&repo.git())?;
    git::init_bare(&repo.git())?;
    tree.link_clone(&repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::optin::test_support::{RollBuilding, RollOf, checkout_with_commit, write_target};

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
        assert_eq!(current, prepared.tree.release(&prepared.sha));
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

    /// fails if a sheep the shepherd already runs from `current` is treated
    /// as one to take over. That is what a cutover that landed and could not
    /// write its record leaves, and the old refusal told the operator to
    /// remove the tree the service runs from. The record is repaired from
    /// what `current` names and the sheep is refused as the target it is.
    #[tokio::test]
    async fn a_sheep_already_running_from_current_has_its_record_repaired() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "bpm");
        let release = tree.release(fixtures::SHA);
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
        let release = tree.release(fixtures::SHA);
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
        let release = tree.release(fixtures::SHA);
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
            shown.contains("shep-deploy setup bpm"),
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
}
