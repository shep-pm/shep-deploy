//! Everything between a sha and its build: choosing the head to deploy,
//! checking it out, and refusing a configuration that could not verify it.

use std::fs;
use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::build::BuildSpec;
use crate::error::Error;
use crate::hook::Hooks;
use crate::paths::{BuildKey, Tree};
use crate::settings::Settings;
use crate::state::Verify;
use crate::{git, linkcheck, shared};

/// What [`stage_release`] hands its callers: the app to register, the build
/// to run and the hooks to run after it (both from the sheep's
/// [`Settings`]), the paths linked in from the operator's checkout, which
/// [`linkcheck::Snapshot`] needs around the build, the build cache the
/// release's `target` was linked at, the release directory and its completion
/// marker, and whether that release was already built when this staged it.
///
/// The cache is here rather than re-derived at each use so the directory the
/// build writes through, the one its artifacts are confined to, and the one
/// the hooks are handed are the same path, decided once, where the link was
/// made. The release and its marker are here for the same reason: every step
/// from here to the swap has to name one directory, and a release shared with
/// another sheep cannot be derived from this tree.
pub(crate) struct Staged {
    pub(crate) app: AppConfig,
    pub(crate) spec: BuildSpec,
    pub(crate) hooks: Hooks,
    pub(crate) linked: shared::FromCheckout,
    pub(crate) cache: PathBuf,
    /// The worktree this deploy builds in and, if it lands, swaps in.
    pub(crate) release: PathBuf,
    /// The file that says `release` is checked out and built.
    pub(crate) marker: PathBuf,
    /// The key `release` and `marker` are named by.
    pub(crate) build: BuildKey,
    /// `release` was found carrying that marker, so its build can be skipped.
    pub(crate) reused: bool,
}

/// The key of the release `settings` builds `app`'s script into, and the three
/// paths that key names.
fn staged_paths(
    tree: &Tree,
    remote: &str,
    sha: &str,
    app: &AppConfig,
    settings: &Settings,
) -> (BuildKey, PathBuf, PathBuf, PathBuf) {
    let build = settings.build_key(app.user.as_deref());
    (
        build.clone(),
        tree.release(remote, sha, &build),
        tree.completion(remote, sha, &build),
        tree.cache_target(remote),
    )
}

/// Everything between a sha and its build: the worktree, the cache link, and
/// the operator's shared files.
///
/// `remote` picks the release directory and the cache, both keyed per remote
/// and not per sheep, and must be the one this tree's clone serves.
///
/// One function because two callers, [`super::attempt`] and
/// [`crate::optin::prepare_with`], had the same seven lines, and because all of
/// it is blocking work (git, symlinks) that both run through
/// [`shared::off_thread`]. Answers a [`Staged`] whose app is the sheep's
/// stored `app`, and whose build and hooks are `settings`'. Nothing in the
/// release is read.
///
/// A release already checked out gets its `target` relinked here, which is the
/// only thing that makes a retry of a pre-upgrade release build: the cache it
/// was linked at moved, and the build is confined to the one it moved to. See
/// [`shared::link_cache`].
///
/// # Errors
/// Whatever [`checkout_release`], [`shared::link_cache`], [`shared::to_link`]
/// and [`shared::link_into`] return.
pub(crate) fn stage_release(
    tree: &Tree,
    checkout: &Path,
    sha: &str,
    remote: &str,
    app: &AppConfig,
    settings: &Settings,
) -> Result<Staged, Error> {
    let (build, release, marker, cache) = staged_paths(tree, remote, sha, app, settings);
    let reused = checkout_release(tree, remote, sha, &build)?;
    shared::link_cache(&release, &cache, &tree.superseded_cache())?;
    let linked = shared::to_link(checkout)?;
    shared::link_into(&release, checkout, &linked)?;

    let staged = Staged {
        app: app.clone(),
        spec: settings.build_spec(),
        hooks: settings.hooks(),
        linked,
        cache,
        release,
        marker,
        build,
        reused,
    };
    // Printed here rather than returned, as `build::root_warning` is: nothing
    // is refused, and `deploy` and `setup` both come through this function.
    for warning in linkcheck::warnings(tree.sheep(), &staged.linked, &staged.spec.artifacts) {
        eprintln!("{warning}");
    }
    Ok(staged)
}

/// What [`stage_release`] answers for a release it built earlier: the
/// operator's shared files are linked in again, with no checkout and no cache
/// link. Its `cache` is the one that link was made at, which is what the
/// hooks and the artifact copies run against.
///
/// # Errors
/// As [`stage_release`], less the checkout and the cache link.
pub(crate) fn read_staged(
    tree: &Tree,
    checkout: &Path,
    sha: &str,
    remote: &str,
    app: &AppConfig,
    settings: &Settings,
) -> Result<Staged, Error> {
    let (build, release, marker, cache) = staged_paths(tree, remote, sha, app, settings);
    let linked = shared::to_link(checkout)?;
    shared::link_into(&release, checkout, &linked)?;
    Ok(Staged {
        app: app.clone(),
        spec: settings.build_spec(),
        hooks: settings.hooks(),
        linked,
        cache,
        release,
        marker,
        build,
        reused: true,
    })
}

/// Writes the marker saying a staged release is checked out and built.
///
/// After its build, never before: the marker is what lets the next deploy of
/// this sha skip it, and a release whose build failed has nothing to skip.
///
/// # Errors
/// [`Error::Io`] if the marker's directory or the marker itself cannot be
/// written.
pub(crate) fn mark_built(marker: &Path) -> Result<(), Error> {
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }
    fs::write(marker, b"").map_err(Error::at(marker))
}

// `release.exists()` is not the check, and never was. `git worktree add`
// writes a commit's files one at a time, so a kill partway leaves the directory
// present holding an arbitrary subset of them. Measured on an 800-file
// repository, three SIGKILLs 12ms in left 249, 358 and 0 files, and the first
// of those left a readable HEAD at the right sha - so `rev-parse` is not the
// missing check either.
//
// What the marker covers is a killed process, not a lost power supply. Nothing
// syncs the checkout before it is written, which is git's job rather than this
// crate's.

/// Checks `sha` out into this tree's release for `build`, unless a FINISHED
/// release is already there, and answers whether it was.
///
/// "Finished" means the completion marker is there and the directory is too.
/// BOTH, not either: the marker alone would say a release is finished after
/// retention had removed it, and the directory alone is what this function
/// exists to stop trusting.
///
/// The marker is written by [`mark_built`], after a build has exited zero, so
/// a release carrying it has been checked out AND built, which is what lets
/// [`super::attempt`] skip the build for it. A release whose build failed
/// carries nothing, and its next attempt starts from a fresh checkout.
///
/// # Errors
/// [`Error::Io`] if a stale directory cannot be removed, the release directory
/// cannot be made, or the marker cannot be written. Whatever
/// [`git::worktree_prune`] and [`git::worktree_add`] return.
fn checkout_release(tree: &Tree, remote: &str, sha: &str, build: &BuildKey) -> Result<bool, Error> {
    let git_dir = tree.git();
    let (release, marker) = (
        tree.release(remote, sha, build),
        tree.completion(remote, sha, build),
    );

    if marker.is_file() && release.is_dir() {
        return Ok(true);
    }

    if release.exists() {
        fs::remove_dir_all(&release).map_err(Error::at(&release))?;
    }
    // Removing the directory is not enough on its own: git still has the
    // path registered and refuses the next add with "missing but already
    // registered worktree". Verified 2026-08-28. Also when the directory was
    // already gone, which is what a release removed by hand leaves.
    git::worktree_prune(&git_dir)?;

    // The releases directory belongs to the repository, and a tree that has
    // never built from this remote has no such directory yet.
    let directory = release.parent().unwrap_or(&release);
    fs::create_dir_all(directory).map_err(Error::at(directory))?;
    git::worktree_add(&git_dir, &release, sha)?;
    Ok(false)
}

/// The sha `branch` (a branch name, or a full ref) points at in the tree's
/// bare clone, with the one failure that means "the branch is gone" said in
/// words.
///
/// Only that one failure is reworded, matching what
/// [`crate::git::current_branch`] does for a detached `HEAD`: `git
/// rev-parse --verify` answers a missing ref with `fatal: Needed a single
/// revision`, which names the branch without ever saying the branch is not
/// there. Every other failure - the bare clone is not a repository at all,
/// say - is left exactly as git reported it, so a real breakage is never
/// mislabelled as a deleted branch.
///
/// The fetch that precedes this call prunes, so a branch deleted upstream
/// really has disappeared from the clone by the time this runs. That is the
/// whole point of pruning: without it this would keep resolving a stale sha
/// and the target would stall silently forever.
pub(crate) fn head_of(tree: &Tree, branch: &str) -> Result<String, Error> {
    let sheep = tree.sheep();
    match git::remote_head(&tree.git(), branch) {
        Ok(sha) => Ok(sha),
        Err(Error::Git {
            command,
            status,
            stderr,
        }) if stderr.contains("Needed a single revision") => {
            let kind = if branch.starts_with("refs/") {
                "ref"
            } else {
                "branch"
            };
            Err(Error::Git {
                command,
                status,
                stderr: format!(
                    "the fetch succeeded but the remote has no {kind} named {branch:?} - it was \
                     deleted or renamed upstream, or never existed - so there is nothing to \
                     deploy (to follow another branch, run `shep-deploy deploy {sheep} --branch \
                     <name>`): {stderr}"
                ),
            })
        }
        Err(other) => Err(other),
    }
}

/// Refuses a `Probed` deploy of a sheep whose readiness is not gated by
/// anything.
///
/// `Probed` means "wait for the sheep to reach `Online`", and `Online` is
/// only a health verdict when something gates it. shep has two such gates
/// and either will do: a `readiness_probe`, or `wait_ready`, which makes it
/// wait for the app to say so on the shepherd channel. With neither, shep
/// waits out the app's `listen_timeout` and calls the process `Online`
/// because it has not died yet, so a `Probed` deploy verifies nothing while
/// claiming to verify the most.
///
/// Refused before the build and long before the swap, so a misconfigured
/// target costs an operator a message rather than a deploy. The message
/// names all three ways out, `verify = "alive"` among them, because that is
/// the deliberate, visible downgrade the design offers rather than a synonym
/// for going without a check.
///
/// It reads the sheep's stored config, which is the app the shepherd runs, so
/// the gate it finds is the gate a reload uses.
///
/// # Errors
/// [`Error::Config`] naming the sheep and every way out.
pub(crate) fn refuse_ungated_verification(
    sheep: &str,
    app: &AppConfig,
    mode: Verify,
) -> Result<(), Error> {
    refuse_ungated(
        sheep,
        app,
        mode,
        "set `verify = \"alive\"` in its `[app.dogs.deploy]` table",
    )
}

/// As [`refuse_ungated_verification`], for `setup`, where the table has no
/// `verify` yet and the way to `alive` is the flag that writes it.
///
/// # Errors
/// [`Error::Config`] naming the sheep and every way out.
pub(crate) fn refuse_ungated_at_setup(
    sheep: &str,
    app: &AppConfig,
    mode: Verify,
) -> Result<(), Error> {
    refuse_ungated(
        sheep,
        app,
        mode,
        &format!("run `shep-deploy init {sheep} --verify alive`"),
    )
}

fn refuse_ungated(
    sheep: &str,
    app: &AppConfig,
    mode: Verify,
    accept_alive: &str,
) -> Result<(), Error> {
    if mode == Verify::Probed && app.readiness_probe.is_none() && !app.wait_ready {
        return Err(Error::Config(format!(
            "{sheep} has verify = \"probed\" but neither a readiness_probe nor wait_ready, so \
             there is nothing for a deploy to wait on: shep reports a sheep with no readiness \
             gate Online as soon as it has not died, which would verify every release, including \
             a broken one. Add a [readiness_probe] to its app config, or set wait_ready if the app \
             announces itself on the channel, or {accept_alive} to accept the weaker check \
             deliberately."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::deploy::test_support::{
        FIXTURE_TABLE, Shepherd, commit_on_origin, fixture_with_previous_release,
    };
    use crate::deploy::{Outcome, deploy};
    use crate::fixtures;
    use crate::paths;
    use crate::swap;

    /// fails if a marker the DEPLOYED REPOSITORY committed is trusted.
    ///
    /// The completion marker used to live inside the release, which made it a
    /// path the repository can write. `git worktree add` writes a tracked file
    /// out as part of the checkout it is meant to vouch for, so a repository
    /// committing `.shep-complete` hands every partial checkout its own
    /// certificate: kill the add partway and the next tick finds the marker,
    /// skips the checkout, and deploys the fragment. That is the round-9
    /// blocker with the adversary holding the pen.
    ///
    /// The marker lives under the tree's own root now, keyed by sha, where
    /// only this dog writes. A repository can still commit a file by that name
    /// and it means nothing.
    #[tokio::test(start_paused = true)]
    async fn a_completion_marker_committed_by_the_repository_is_not_trusted() {
        let mut fixture = fixture_with_previous_release();
        // The name the marker used to have, which is the point: it is not
        // special any more, and a repository committing it means nothing.
        fs::write(fixture.origin.path().join(".shep-complete"), "")
            .expect("the repository's own marker");
        fixtures::run_git(fixture.origin.path(), &["add", "-A"]);
        fixtures::run_git(
            fixture.origin.path(),
            &["commit", "-q", "-m", "a forged marker"],
        );
        let second = commit_on_origin(&fixture, "second.txt");

        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch");
        let release = fixture.release(&second);
        crate::git::worktree_add(&fixture.tree.git(), &release, &second).expect("worktree");
        assert!(
            release.join(".shep-complete").is_file(),
            "the repository's marker must really be in the checkout, or this proves nothing"
        );
        fs::remove_file(release.join("second.txt")).expect("a checkout that did not finish");

        deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert!(
            release.join("second.txt").exists(),
            "a marker the repository wrote must not vouch for a partial checkout"
        );
    }

    /// fails if a release directory is deployed without anyone checking that
    /// the checkout inside it finished.
    ///
    /// `git worktree add` writes a commit's files one at a time, so a kill
    /// partway leaves the directory present holding an arbitrary subset. The
    /// test used to be `release.exists()`, so the next tick against that sha
    /// skipped the checkout, then built, swapped, reloaded and verified
    /// against the partial tree and reported success.
    ///
    /// Modelled by checking the release out properly and removing one file,
    /// which is what a real kill leaves. Measured on an 800-file repository,
    /// three SIGKILLs 12ms in left 249, 358 and 0 files, and the first of
    /// those had a readable HEAD at the right sha - so `rev-parse` is not the
    /// missing check either.
    ///
    /// Checking out here needs its own fetch: the bare clone does not have
    /// `second` until `deploy` fetches, and this has to place the directory
    /// BEFORE deploy runs.
    #[tokio::test(start_paused = true)]
    async fn a_release_whose_checkout_did_not_finish_is_checked_out_again() {
        let mut fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");

        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch so the sha is checkoutable");
        let release = fixture.release(&second);
        crate::git::worktree_add(&fixture.tree.git(), &release, &second).expect("worktree");
        fs::remove_file(release.join("second.txt")).expect("a checkout that did not finish");

        let outcome = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert_eq!(
            outcome,
            Outcome::Deployed {
                sha: second.clone()
            }
        );
        assert!(
            release.join("second.txt").exists(),
            "a deployed release must hold every file of its commit"
        );
    }

    /// fails if the app a release is staged with comes from the release. A
    /// committed `Flockfile.toml` that changes `script` and sets `user` is in
    /// the checkout, and the staged app is still the sheep's stored one.
    #[tokio::test]
    async fn the_staged_app_is_the_stored_one_whatever_the_release_carries() {
        let fixture = fixture_with_previous_release();
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './elsewhere.sh'\nuser = 'root'\n",
        )
        .expect("write Flockfile");
        let second = commit_on_origin(&fixture, "second.txt");
        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch");
        let stored = fixtures::gated_app("web", FIXTURE_TABLE);
        let settings = crate::settings::from_app("web", &crate::fixtures::no_section(), &stored)
            .expect("settings");

        let staged = super::stage_release(
            &fixture.tree,
            &fixture.state.checkout,
            &second,
            &fixture.state.remote,
            &stored,
            &settings,
        )
        .expect("staged");

        let shipped = fs::read_to_string(fixture.release(&second).join("Flockfile.toml"))
            .expect("the release carries its Flockfile");
        assert!(shipped.contains("elsewhere.sh"), "{shipped}");
        assert_eq!(staged.app, stored);
        assert_eq!(staged.app.script, "./run.sh");
        assert_eq!(staged.app.user, None);
    }

    /// fails if a release links `target` at a cache of its own, or at one the
    /// build then writes through a different path. Two sheep of one remote
    /// would build the same tree once each, and the artifacts a build leaves
    /// in the cache would be copied from a directory nothing linked.
    #[test]
    fn a_release_links_target_at_the_repository_cache() {
        let fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch");
        let stored = fixtures::gated_app("web", FIXTURE_TABLE);
        let settings = crate::settings::from_app("web", &crate::fixtures::no_section(), &stored)
            .expect("settings");

        let staged = super::stage_release(
            &fixture.tree,
            &fixture.state.checkout,
            &second,
            &fixture.state.remote,
            &stored,
            &settings,
        )
        .expect("staged");

        let link = fixture.release(&second).join("target");
        assert_eq!(fs::read_link(&link).expect("a target link"), staged.cache);
        assert!(staged.cache.is_dir(), "the cache exists before the link");

        let repos = fixture.home.path().join("deploy").join(paths::REPOS);
        assert!(
            staged.cache.starts_with(&repos),
            "{:?} under {repos:?}",
            staged.cache
        );
        assert!(
            !staged.cache.starts_with(fixture.tree.root()),
            "the cache is not inside the tree: {:?}",
            fixture.tree.root()
        );
    }

    /// fails if a release checked out BEFORE the cache moved cannot be built
    /// again. `checkout_release` reuses a finished checkout and `link_cache`
    /// used to return early for any `target` already there, so the first
    /// attempt at a pre-upgrade release after the upgrade carried a link at the
    /// cache that used to live in the tree while `Staged.cache`, and with it
    /// both roots the artifacts are confined to, named the one beside the clone.
    /// An artifact spelled `target/release/app` then resolves outside both and
    /// `copy_artifact` refuses it: a release that used to deploy fails every
    /// time it is retried.
    #[test]
    fn a_release_checked_out_before_the_cache_moved_is_relinked() {
        let fixture = fixture_with_previous_release();
        let second = commit_on_origin(&fixture, "second.txt");
        crate::git::fetch(
            &fixture.tree.git(),
            &fixture.state.remote,
            "main",
            fixtures::TEST_BUDGET,
        )
        .expect("fetch");

        // The release as the previous version left it: a finished checkout,
        // its marker, and `target` linked at the cache inside the tree.
        let (release, old) = (
            fixture.release(&second),
            fixture.tree.superseded_cache().join("target"),
        );
        crate::git::worktree_add(&fixture.tree.git(), &release, &second).expect("worktree");
        fs::create_dir_all(fixture.completions()).expect("markers");
        fs::write(fixture.completion(&second), "").expect("the checkout finished");
        fs::create_dir_all(&old).expect("the cache the old link names");
        std::os::unix::fs::symlink(&old, release.join("target")).expect("the old link");

        let stored = fixtures::gated_app("web", FIXTURE_TABLE);
        let settings = crate::settings::from_app("web", &crate::fixtures::no_section(), &stored)
            .expect("settings");
        let staged = super::stage_release(
            &fixture.tree,
            &fixture.state.checkout,
            &second,
            &fixture.state.remote,
            &stored,
            &settings,
        )
        .expect("staged");

        assert_eq!(
            fs::read_link(release.join("target")).expect("the link is the new one"),
            staged.cache,
            "the reused release builds against the cache its build is confined to"
        );
        assert!(
            old.is_dir(),
            "the old cache is left where a live release may still be serving from it"
        );
    }

    /// fails if a `probed` target with no readiness probe is deployed
    /// anyway. `Online` is only a health verdict when a probe is what gates
    /// it; without one shep waits out `listen_timeout` and reports a
    /// process `Online` for not having died, so this configuration
    /// verifies every release including a broken one. The spec requires the
    /// refusal by name.
    #[tokio::test]
    async fn a_probed_target_with_no_readiness_probe_is_refused() {
        let mut fixture = fixture_with_previous_release();
        let previous = fixture.state.deployed.clone().expect("a previous release");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready().with_app(fixtures::app_with_settings("web", FIXTURE_TABLE));

        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no probe to wait on");

        let shown = err.to_string();
        assert!(shown.contains("readiness_probe"), "{shown}");
        assert!(shown.contains("alive"), "{shown}");
        // Refused before anything was touched, which is why it is worth
        // refusing at all rather than deploying and hoping.
        assert_eq!(daemon.reload_count(), 0, "nothing may be reloaded");
        assert_eq!(
            swap::resolve(&fixture.tree.current()).unwrap(),
            Some(fixture.release(&previous))
        );
    }

    /// fails if the refusal catches a sheep that waits on its channel.
    /// `wait_ready` is shep's other readiness gate and the stronger of the
    /// two: it reports `Online` only once the app says so itself, which is
    /// exactly as meaningful as a probe. Refusing it would name three ways
    /// out of a problem the operator had already solved.
    #[tokio::test(start_paused = true)]
    async fn a_probed_target_that_waits_on_its_channel_is_not_refused() {
        let mut fixture = fixture_with_previous_release();
        let mut app = fixtures::app_with_settings("web", FIXTURE_TABLE);
        app.wait_ready = true;
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::ready().with_app(app),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if the refusal above catches `alive` too. `alive` is the
    /// deliberate downgrade the refusal's own message points at, so a
    /// target that has taken it must deploy without a probe - otherwise
    /// the message names a way out that does not exist.
    #[tokio::test(start_paused = true)]
    async fn an_alive_target_needs_no_readiness_probe() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.verify = crate::state::Verify::Alive;
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::ready().with_app(fixtures::app_with_settings(
                "web",
                "branch = 'main'\nwatch = 'manual'\nverify = 'alive'\n",
            )),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect("completes");

        assert_eq!(outcome, Outcome::Deployed { sha: second });
    }

    /// fails if a branch that is not on the remote comes back as git's own
    /// `fatal: Needed a single revision`, which names the branch without
    /// ever saying the branch is not there. A branch deleted upstream is
    /// the one failure a watched target hits unattended, so it is the one
    /// that most needs to explain itself.
    #[tokio::test]
    async fn a_branch_that_is_not_on_the_remote_says_so() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.branch = "gone".to_owned();

        let err = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no such branch");

        let shown = err.to_string();
        assert!(shown.contains("gone"), "{shown}");
        assert!(shown.contains("no branch named"), "{shown}");
        // Every word of it, not just the words before the first line break
        // in the source. A `\\` where a line continuation was meant puts a
        // literal backslash and seventeen spaces in the middle of the one
        // message a watched target hits unattended, and assertions that
        // stop early never see it.
        assert!(shown.contains("nothing to deploy"), "{shown}");
        assert!(
            shown.contains("shep-deploy deploy web --branch <name>"),
            "{shown}"
        );
        assert!(!shown.contains('\\'), "{shown}");
    }

    /// fails if a tracked full ref that is not on the remote is called a
    /// branch. `refs/heads/gone` is the ref spelling of a branch the mirror
    /// fetch does not fail on, so it reaches the same lookup a deleted branch
    /// does.
    #[tokio::test]
    async fn a_ref_that_is_not_on_the_remote_says_so() {
        let mut fixture = fixture_with_previous_release();
        fixture.state.branch = "refs/heads/gone".to_owned();

        let err = deploy(
            &Shepherd::ready(),
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("no such ref");

        let shown = err.to_string();
        assert!(shown.contains("refs/heads/gone"), "{shown}");
        assert!(shown.contains("no ref named"), "{shown}");
        assert!(!shown.contains("no branch named"), "{shown}");
        assert!(shown.contains("nothing to deploy"), "{shown}");
    }
}
