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
use crate::optin::clone;
use crate::paths::{self, Tree};
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
/// shepherd has no sheep by that name registered, if it records no
/// working directory for it, or if its clone cannot be moved onto the shared
/// one. Otherwise, whatever [`roll::registered`],
/// [`git::remote_url`], [`git::current_branch`], [`super::clone::share_clone`],
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
/// The build takes the build turn for its repository, so a setup of one sheep
/// waits while another sheep of the same remote is building: the cache it builds
/// into is the one they share. It is let go before the table write and the record
/// write. See [`crate::lock`].
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
             that: rename it in its Flockfile and run init again"
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
    // The key cannot be forwarded: shep-core 0.23 never serializes it, so
    // the `Start` would register the sheep outside crabpot, silently.
    if let Some(file) = previous_config.sandbox.as_deref() {
        return Err(Error::Config(format!(
            "{sheep} is registered with `sandbox = {file:?}`, and this dog cannot carry that \
             key to the shepherd: shep-client 0.23 never sends it, so the sheep would run \
             unsandboxed. Move the sandbox to an `[app.dogs.sandbox]` table, or leave the \
             sheep as it is"
        )));
    }
    let checkout = PathBuf::from(previous_config.cwd.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "shep records no working directory for {sheep}, so there is no checkout to deploy \
             from"
        ))
    })?);
    // The shepherd already runs the sheep from this tree's `current`: a
    // cutover landed and its record was not written, or the record was lost
    // afterwards. Either way the tree is live, and the advice the record
    // branch below gives an unrecorded tree, remove it and run init again,
    // would take a running service's cwd with it. The record is repaired
    // from what `current` names instead, and the sheep is then what it is:
    // a deploy target, refused here like any other.
    if shared::same_path(&checkout, &tree.current())
        && let Some(release) = swap::resolve(&tree.current())?
        && let Some(name) = release.file_name().and_then(|name| name.to_str())
        && crate::state::is_sha(crate::paths::release_sha(name))
        && release.is_dir()
    {
        // The sha, not the directory name: a release directory carries the
        // build key too, and a record carrying that would be refused by
        // `State::read` on every later run.
        let sha = crate::paths::release_sha(name);
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
             Flockfile in the checkout it should deploy from, then run init again",
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
                tree.root().display()
            )
        } else {
            format!(
                "{sheep} has a deploy tree at {} but was never cut over to it: its record names \
                 no deployed release, so nothing has ever been served from that tree. An \
                 abandoned first cutover leaves exactly this. Do NOT run `shep deploy {sheep}` \
                 against it - that would reload the sheep at its own checkout and report success \
                 for a release nothing served. Remove that tree and run `shep-deploy init \
                 {sheep}` again once the cause of the first failure is fixed.",
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
                 shep has for {sheep}; fix whichever is named and run init again"
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
    // Unresolved, which is safe here and nowhere else: init runs no hook, so
    // nothing secret-bearing is executed from what this reads.
    let settings = crate::settings::from_app(sheep, &config.shared, &previous_config)?;

    // Same hold a deploy takes, for the same reason: everything below writes
    // to this tree, and a poll tick can be doing the same at the same moment.
    // After every refusal that needs no tree, so a `setup` that fails on a
    // detached HEAD or a missing `origin` leaves nothing behind: `hold` makes
    // the tree's root itself. Carried out in `Prepared` so the cutover runs
    // under it too.
    let hold = lock::hold(&tree)?;

    // Off the runtime's thread: it waits on the clone's lock.
    {
        let (tree, shep_home, remote) = (tree.clone(), shep_home.to_owned(), state.remote.clone());
        shared::off_thread(move || clone::share_clone(&tree, &shep_home, &remote)).await?;
    }
    // Off the runtime's thread, like every git call: see `shared::off_thread`.
    let (git_dir, remote, tracked, budget) = (
        tree.git(),
        state.remote.clone(),
        state.branch.clone(),
        config.git_timeout,
    );
    shared::off_thread(move || git::fetch(&git_dir, &remote, &tracked, budget)).await?;
    let sha = git::remote_head(&tree.git(), &state.branch)?;

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
        app,
        spec,
        linked,
        cache,
        release,
        marker,
        reused,
        ..
    } = {
        let (tree, checkout, sha, remote) = (
            tree.clone(),
            state.checkout.clone(),
            sha.clone(),
            state.remote.clone(),
        );
        let (app, settings) = (previous_config.clone(), settings.clone());
        shared::off_thread(move || {
            crate::deploy::stage_release(&tree, &checkout, &sha, &remote, &app, &settings)
        })
        .await?
    };
    // Before the build, so a refusal costs no build.
    refuse_ungated_at_setup(sheep, &app, verify)?;
    // The build turn for this repository, inside setup's own tree hold and let
    // go before anything is written. Setup builds, and the cache its build
    // writes into is the one every sheep of this remote shares, including the
    // ones a poll tick is deploying. See [`crate::lock`].
    let building = {
        let (tree, remote) = (tree.clone(), state.remote.clone());
        shared::off_thread(move || crate::lock::hold_build(&tree.repo(&remote))).await?
    };
    let snapshot = crate::linkcheck::Snapshot::before(sheep, &release, &linked);
    // A release another sheep of this remote already built is not built twice
    // here either. See [`crate::deploy::prepare`].
    let built = if reused {
        Ok(())
    } else {
        build::run(
            sheep,
            &release,
            &spec,
            app.user.as_deref(),
            &config.passthrough,
            &cache,
            config.build_timeout,
        )
        .await
    };
    for warning in snapshot.after() {
        eprintln!("{warning}");
    }
    // Let go before the table write and the record write below, so nothing
    // holding the build turn ever waits for a record lock.
    drop(building);
    built?;
    if !reused {
        crate::deploy::mark_built(&marker)?;
    }

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

#[cfg(test)]
mod tests;
