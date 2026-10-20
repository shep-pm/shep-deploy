//! The disruptive half of a deploy: everything from the swap of `current` to
//! the verdict, and the release hook that runs just before it.
//!
//! Split from the build so [`crate::stage::promote`] can reach it without
//! one: a staged release was built when it was staged, and lands here when
//! the operator says.

use std::path::PathBuf;

use crate::config::DogConfig;
use crate::daemon::Daemon;
use crate::deploy::reload::{Landed, land, named};
use crate::deploy::rollback::{roll_back, undo_swap};
use crate::deploy::{Outcome, Staged, record_landing};
use crate::error::Error;
use crate::paths::Tree;
use crate::state::State;
use crate::{hook, swap};

/// Runs the release hook over `head`'s release, which is built.
///
/// Called after the build and before `started_at` is compared with `current`
/// in [`swap_and_land`], so the `Raced` check covers the hook's run time as
/// well as the build's.
///
/// # Errors
/// Whatever [`hook::run`] returns.
pub(crate) async fn run_release_hook(
    tree: &Tree,
    head: &str,
    staged: &Staged,
    config: &DogConfig,
) -> Result<(), Error> {
    hook::run(
        &staged.hooks.release,
        &hook::Context {
            sheep: tree.sheep(),
            sha: head,
            release: &tree.release(head),
            as_user: staged.app.user.as_deref(),
            passthrough: &config.passthrough,
            budget: config.build_timeout,
            cache: &tree.cache_target(),
            built: staged.spec.command.is_some(),
        },
    )
    .await
}

/// Swaps `current` to `head`'s release, reloads the sheep, verifies it, and
/// undoes the swap if it does not come up.
///
/// `started_at` is what `current` named when the caller began, and the swap is
/// refused if it names something else now. `staged` is what
/// [`super::stage_release`] read from the release.
///
/// # Errors
/// [`Error::Raced`] if `current` moved since `started_at` was read. Then as
/// the tail of [`super::deploy`]: see its `# Errors` for the rollback
/// endings, which are the same here.
pub(crate) async fn swap_and_land<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    state: &mut State,
    head: &str,
    started_at: Option<PathBuf>,
    staged: &Staged,
    config: &DogConfig,
) -> Result<Outcome, Error> {
    let sheep = tree.sheep();
    let release = tree.release(head);
    let Staged {
        app, spec, hooks, ..
    } = staged;

    // Nothing above here has touched the running app. Everything below has.
    //
    // Both sides of this comparison are `read_link` of the same file by the
    // same process, so they are the same spelling of any path that has not
    // changed - unlike `roll_back`'s check, which sees text from two
    // sources and compares shas for that reason.
    let previous = swap::resolve(&tree.current())?;
    if previous != started_at {
        return Err(Error::Raced {
            sheep: sheep.to_owned(),
            started: named(started_at.as_deref()),
            found: named(previous.as_deref()),
        });
    }
    swap::point_at(&tree.current(), &release)?;

    // THE BOUNDARY. Every step from here that can fail and might need
    // undoing lives inside `land`, so that this one `match` is the only
    // place that decides what to undo. The boundary used to sit around a
    // reload-and-verify call with the generation capture outside it, and
    // that one line outside was enough to leave `current` on a release
    // nothing had verified: see [`land`].
    match land(daemon, sheep, app, state.verify).await {
        // The record write below is the one fallible thing past the swap
        // that `land` does not own, and it is deliberate rather than a
        // leak. It runs only after a verify has SUCCEEDED, so the new
        // release is up and serving: there is nothing to undo, and undoing
        // it would take a healthy release out of service. Its failure is
        // printed and the outcome stays `Deployed`, because that is what
        // happened. Returning the failure instead had `record` write the
        // sha into `failed` next to the same sha in `deployed`: a record
        // that said the release was serving and held at once.
        Landed::Verified => {
            // After the record write, so the record names the new sha even
            // when the hook fails, and only when it does: a hook that reads
            // `deploy.toml` would otherwise see the previous sha. Like the
            // write, the hook cannot change the outcome: the release is live
            // and verified, and undoing it would take a healthy release out
            // of service.
            match record_landing(tree, state, head) {
                Ok(()) => {
                    if let Err(err) = hook::run_after(
                        &hooks.after,
                        &hook::Context {
                            sheep,
                            sha: head,
                            release: &release,
                            as_user: app.user.as_deref(),
                            passthrough: &config.passthrough,
                            budget: config.build_timeout,
                            cache: &tree.cache_target(),
                            built: spec.command.is_some(),
                        },
                    )
                    .await
                    {
                        eprintln!(
                            "shep-deploy: {sheep}: deployed {head}, but the after hook failed: \
                             {err}"
                        );
                    }
                }
                Err(err) => {
                    eprintln!(
                        "shep-deploy: {sheep}: deployed {head}, but could not record it: {err}"
                    );
                    if hooks.after.command.is_some() {
                        eprintln!(
                            "shep-deploy: {sheep}: the after hook was skipped, because the record \
                             does not name {head}"
                        );
                    }
                }
            }
            Ok(Outcome::Deployed {
                sha: head.to_owned(),
            })
        }
        // Nothing was ever started on the new release, so there is nothing
        // to reload onto the old one - only a symlink to put back.
        Landed::NotStarted(source) => {
            undo_swap(tree, state, previous.as_deref(), head, &source)?;
            Err(source)
        }
        Landed::NotVerified { why, patience } => {
            let to = roll_back(
                daemon,
                tree,
                state,
                app,
                previous.as_deref(),
                head,
                &why,
                patience,
            )
            .await?;
            Ok(Outcome::RolledBack { to, why })
        }
        // The same rollback as above, not a lesser one. This error arrived
        // after a reload the shepherd accepted, so the new release may be
        // running: shep spawns the replacement, waits for it, and drains
        // the old instance, and only then did a `describe` fail. Moving a
        // symlink does not move a running process, so a swap without a
        // reload would leave the daemon on the new code while `current`
        // and `deploy.toml` both name the old one.
        Landed::Failed { source, patience } => {
            let to = roll_back(
                daemon,
                tree,
                state,
                app,
                previous.as_deref(),
                head,
                &source.to_string(),
                patience,
            )
            .await?;
            Err(Error::RolledBack {
                to,
                source: Box::new(source),
            })
        }
    }
}
