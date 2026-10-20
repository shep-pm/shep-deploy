//! Everything between a sha and its build: choosing the head to deploy,
//! checking it out, and refusing a configuration that could not verify it.

use std::fs;
use std::path::Path;

use shep_client::shep_core::config::AppConfig;

use crate::build::BuildSpec;
use crate::error::Error;
use crate::hook::Hooks;
use crate::paths::Tree;
use crate::state::Verify;
use crate::{flockfile, git, linkcheck, shared};

/// What [`stage_release`] hands its callers: the app to register, the build
/// to run, the hooks to run after it, and the paths linked in from the
/// operator's checkout, which [`linkcheck::Snapshot`] needs around the build.
pub(crate) struct Staged {
    pub(crate) app: AppConfig,
    pub(crate) spec: BuildSpec,
    pub(crate) hooks: Hooks,
    pub(crate) linked: shared::FromCheckout,
}

/// Everything between a sha and its build: the worktree, the cache link,
/// the operator's shared files, and the two Flockfiles read once.
///
/// One function because two callers, [`super::attempt`] and
/// [`crate::optin::prepare_with`], had the same seven lines, and because all of
/// it is blocking work (git, symlinks, file reads) that both run through
/// [`shared::off_thread`]. Answers a [`Staged`].
///
/// # Errors
/// Whatever [`checkout_release`], [`shared::link_cache`],
/// [`shared::to_link`], [`shared::link_into`] and [`flockfile::read`]
/// return, plus the app-selection and schema failures of
/// [`flockfile::Merged::app_config`], [`flockfile::Merged::build_spec`] and
/// [`flockfile::Merged::hooks`].
pub(crate) fn stage_release(tree: &Tree, checkout: &Path, sha: &str) -> Result<Staged, Error> {
    let release = tree.release(sha);
    checkout_release(tree, sha)?;
    shared::link_cache(&release, &tree.cache_target())?;
    // Held, not recomputed. This is the only record of which files came from
    // the operator's own checkout rather than from the repository, and
    // `flockfile` needs it to know whether an override is theirs.
    let shared_paths = shared::to_link(checkout)?;
    shared::link_into(&release, checkout, &shared_paths)?;

    let flockfiles = flockfile::read(&release, &shared_paths)?;
    let app = flockfiles.app_config(tree.sheep())?;
    let spec = flockfiles.build_spec()?;
    let hooks = flockfiles.hooks()?;
    // Printed here rather than returned, as `build::root_warning` is: nothing
    // is refused, and `deploy` and `setup` both come through this function.
    for warning in linkcheck::warnings(tree.sheep(), &shared_paths, &spec.artifacts) {
        eprintln!("{warning}");
    }
    Ok(Staged {
        app,
        spec,
        hooks,
        linked: shared_paths,
    })
}

/// Checks `sha` out at `release`, unless a FINISHED checkout is already there.
///
/// `release.exists()` was the test until round 9 of the founder's review, and
/// it cannot tell a finished checkout from one that died partway. `git
/// worktree add` writes a commit's files one at a time, so a kill leaves the
/// directory present holding an arbitrary subset of them. Measured on an
/// 800-file repository, three SIGKILLs 12ms in left 249, 358 and 0 files.
///
/// The next tick against that sha then skipped the checkout and built,
/// swapped, reloaded and verified against the subset, reporting success.
/// Nothing downstream catches it. A missing `Flockfile.toml` fails loudly,
/// and every other missing file is simply absent from a release the operator
/// has been told is live.
///
/// `rev-parse HEAD` is not the check either, which is worth writing down
/// because it is the obvious one to reach for. The first of those three kills
/// left a readable HEAD at the right sha next to 249 of 800 files.
///
/// So the marker is written here, after `git worktree add` returns, and only
/// a release carrying it is reused.
///
/// What it covers is a killed process, not a lost power supply. Nothing
/// syncs the checkout before the marker is written, so a crash of the host
/// can leave the marker durable and part of the checkout not; the next
/// deploy of that sha would then reuse it. Closing that needs a sync of
/// every file the worktree wrote, which is git's job rather than this
/// crate's, and a host that loses power mid-deploy has larger problems than
/// one release.
///
/// # Errors
/// [`Error::Io`] if a stale directory cannot be removed or the marker cannot
/// be written. Whatever [`git::worktree_prune`] and [`git::worktree_add`]
/// return.
fn checkout_release(tree: &Tree, sha: &str) -> Result<(), Error> {
    let git_dir = tree.git();
    let release = tree.release(sha);
    let marker = tree.completion(sha);

    // BOTH, not either. The marker alone would say a release is finished after
    // retention had removed it, and the directory alone is what this function
    // exists to stop trusting.
    if marker.is_file() && release.exists() {
        return Ok(());
    }

    if release.exists() {
        fs::remove_dir_all(&release).map_err(Error::at(&release))?;
        // Removing the directory is not enough on its own: git still has the
        // path registered and refuses the next add with "missing but already
        // registered worktree". Verified 2026-08-28.
        git::worktree_prune(&git_dir)?;
    }

    git::worktree_add(&git_dir, &release, sha)?;

    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }
    fs::write(&marker, b"").map_err(Error::at(marker))
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
pub(super) fn head_of(tree: &Tree, branch: &str) -> Result<String, Error> {
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
/// # This is a heuristic, and it cannot be more
///
/// It reads the app definition from the release's own Flockfile, which is
/// not necessarily the config the shepherd is running. Nothing in this crate
/// re-registers a sheep, so the readiness gate a reload actually uses is
/// whatever was registered when the sheep was started, and
/// [`crate::daemon::Daemon::describe`] reports status, pid and uptime - never config. There
/// is no request that would answer the real question.
///
/// So the check catches the case worth catching, a target whose Flockfile
/// declares no gate at all, and misses the asymmetric one: a sheep
/// REGISTERED without a gate whose release Flockfile has since added one
/// passes here and then gets `listen_timeout`-elapsed verification under the
/// `probed` label. The turnover requirement in [`crate::verify`] still
/// applies to it, so what it loses is the readiness half of the check, not
/// the whole of it.
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
        "change the verify line in deploy.toml to \"alive\"",
    )
}

/// As [`refuse_ungated_verification`], for `setup`, where no `deploy.toml`
/// exists yet and the way to `alive` is the flag that writes it.
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
        &format!("run `shep-deploy setup {sheep} --verify alive`"),
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
             a broken one. Add a [readiness_probe] to its Flockfile, or set wait_ready if the app \
             announces itself on the channel, or {accept_alive} to accept the weaker check \
             deliberately."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::deploy::test_support::{Shepherd, commit_on_origin, fixture_with_previous_release};
    use crate::deploy::{Outcome, deploy};
    use crate::fixtures;
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
        let release = fixture.tree.release(&second);
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
        let release = fixture.tree.release(&second);
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
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\n",
        )
        .expect("write a probeless Flockfile");
        commit_on_origin(&fixture, "second.txt");
        let daemon = Shepherd::ready();

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
            Some(fixture.tree.release(&previous))
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
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\nwait_ready = true\n",
        )
        .expect("write a channel-gated Flockfile");
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::ready(),
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
        fs::write(
            fixture.origin.path().join("Flockfile.toml"),
            "[[app]]\nname = 'web'\nscript = './run.sh'\n",
        )
        .expect("write a probeless Flockfile");
        let second = commit_on_origin(&fixture, "second.txt");

        let outcome = deploy(
            &Shepherd::ready(),
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
