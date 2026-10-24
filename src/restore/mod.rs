//! On removal, put every sheep back where it ran before this dog took over.
//!
//! shep runs [`all`] as this dog's own `on-remove` hook, once, before
//! forgetting it. There is no next tick and no retry beyond what is written
//! here, so a failure has to be loud rather than swallowed.
//!
//! # The failure this prevents
//!
//! An operator rehomes the dog, goes back to `~/ReactMap` because that is
//! where they think their app lives, restarts the sheep, and cannot work
//! out why nothing updates. That sheep's `cwd` was never `~/ReactMap`; it
//! was a path under `$SHEP_HOME` they have no reason to know about.
//! [`State::origin_cwd`] and [`State::origin_script`], captured once at
//! opt-in, are where this module puts it back.
//!
//! # Two cases, both answered from `deploy.toml`
//!
//! A sheep that pre-existed the dog has `origin_cwd` and `origin_script`
//! and is restored to them. A sheep the dog bootstrapped has neither, so
//! there is nothing to restore: it is left running from `current`,
//! unchanged, and the report says so plainly. Deleting an app because a
//! deploy tool was uninstalled would be far worse than leaving it.
//!
//! # Why there is a fallback, and why one outcome is worse than the others
//!
//! [`Request::Delete`] is stop plus deregister, and `FlockRegistry::roll`
//! drops a name with no live instance, so delete-then-start is destructive
//! if the start is refused: the sheep would be gone from the flock AND the
//! roll, not returning on a reboot. A refused restore is retried with the
//! config the shepherd had a moment ago, which covers the common causes -
//! a transient refusal, a bad `origin_script`, a `user` that no longer
//! resolves - at the cost of one extra request. Only when that fallback
//! also fails is the sheep genuinely gone, and [`Restored::Lost`] says so
//! in words an operator cannot misread as "still running".
//!
//! # What is left on disk
//!
//! The deploy tree stays, since a sheep may still be running from it. A sheep
//! that is put back lets go of the clone it shared with other sheep with the
//! same remote (see [`crate::paths::Repo`]), and the clone goes with the last
//! one: [`crate::retention::release_clone`].
//!
//! [`Request::Delete`]: shep_client::shep_core::protocol::Request::Delete

mod put_back;

use std::path::{Path, PathBuf};

use crate::daemon::Daemon;
use crate::paths::{self, Tree};
use crate::retention;
use crate::roll;
use crate::state::State;
use put_back::{PutBack, put_back, registered_against};

/// What became of one sheep during removal.
#[derive(Debug)]
pub enum Restored {
    /// Put back at its own checkout, from before this dog took over.
    Returned {
        /// The sheep restored.
        sheep: String,
        /// Where it was put back to.
        to: PathBuf,
    },
    /// A cutover that never landed, so the sheep was never moved and is
    /// already running where it belongs.
    ///
    /// `optin::prepare` writes `origin_cwd` and `origin_script` and then
    /// stops; `cut_over` is what re-registers the sheep against `current`.
    /// Between the two, a tree exists and names an origin for a sheep that
    /// is still running exactly where it always was. Restoring it would
    /// delete and restart a healthy app to put it back where it already is,
    /// and a `Start` that then failed would leave it stopped.
    NeverMoved {
        /// The sheep that was never moved.
        sheep: String,
        /// Where it has been running the whole time.
        at: PathBuf,
    },
    /// The dog bootstrapped this sheep, so there was nowhere to restore it
    /// to. Left running from `current`, unchanged.
    LeftRunning {
        /// The sheep left running.
        sheep: String,
        /// Where it is still running from.
        from: PathBuf,
    },
    /// Nothing was changed by this dog. This says nothing about whether the
    /// sheep is still registered or running: `all` also uses it for a
    /// `deploy.toml` it could not even read, and `put_back` uses it for a
    /// sheep the shepherd no longer has registered at all.
    Failed {
        /// The sheep that could not be restored.
        sheep: String,
        /// Why.
        why: String,
    },
    /// A directory under the deploy root that could not be read as targets
    /// at all: the root itself, or one entry with a name no sheep can have.
    ///
    /// Not a [`Self::Failed`], whose `sheep` names a sheep; this names a
    /// path, and nothing under it was looked at.
    Unlisted {
        /// The directory.
        dir: PathBuf,
        /// Why.
        why: String,
    },
    /// The restore itself was refused, but the fallback put the shepherd's
    /// own previous config back.
    ///
    /// Distinct from [`Self::Failed`] because it is NOT "left as it is":
    /// the delete already ran and `start(vec![current])` then succeeded, so
    /// the sheep was stopped and a fresh instance started. The registered
    /// config an operator reads in `shep flock` now matches what it was a
    /// moment ago, but nothing mid-flight survived the restart to get
    /// there.
    Reset {
        /// The sheep that could not be restored, but was put back running.
        sheep: String,
        /// Why the restore itself failed.
        why: String,
    },
    /// At least one delete already landed for this sheep before another
    /// one refused, so there is still at least one instance to account
    /// for.
    ///
    /// Its own variant because neither of the other failure shapes is true
    /// of it: `Failed`'s "left as it is" is false - at least one instance
    /// really is gone - and `Lost`'s "gone from the flock" over-claims,
    /// since instances the loop never reached may still be serving. What
    /// this crate cannot know from here is whether the registered config
    /// survived: `Delete` is stop plus deregister, and the roll drops a
    /// name with no live instance, so a delete that lands on the LAST
    /// remaining instance really does deregister it. Only `shep describe`
    /// and `shep flock` can say.
    PartlyDeleted {
        /// The sheep left in this state.
        sheep: String,
        /// Why the delete that stopped partway failed.
        why: String,
    },
    /// The delete succeeded, both the restore and the fallback failed, and
    /// the sheep is now gone from the flock AND from the roll.
    Lost {
        /// The sheep that is gone.
        sheep: String,
        /// Why neither the restore nor the fallback could put it back.
        why: String,
    },
    /// The sheep was put back, but the shared clone it linked to could not be
    /// let go of, so it may still be on disk.
    ///
    /// Reported after the sheep's own row, which is true: nothing here
    /// changes where the sheep runs.
    CloneKept {
        /// The sheep that was restored.
        sheep: String,
        /// Why the clone or the link could not be removed.
        why: String,
    },
    /// The muster roll itself could not be read, so none of these
    /// pre-existing sheep could be checked against it at all.
    ///
    /// One row for the whole run rather than one per sheep: the roll is a
    /// single read shared by every target, so its failure is a dog-wide
    /// condition, not a property of any one of them. Repeating "sheep is no
    /// longer registered" once per target would also bury the real cause -
    /// [`roll::read`](crate::roll)'s own crafted, actionable message about
    /// a shepherd newer than this dog - behind a guess this module made up
    /// because it never got to check.
    RollUnreadable {
        /// Every pre-existing sheep this run could not check.
        sheep: Vec<String>,
        /// [`roll::registered`](crate::roll)'s own failure.
        why: String,
    },
}

/// Puts every target back, and answers with one row per target.
///
/// Never returns a `Result`, and that is the contract rather than a
/// convenience: an operator asking to remove something is entitled to have
/// it removed, so a failure here becomes a row in the report and the
/// process still exits 0. A dog that refused to be uninstalled because one
/// of five sheep would not restart would be worse than one that did nothing
/// at all.
pub async fn all<D: Daemon>(daemon: &D, shep_home: &Path) -> Vec<Restored> {
    let found = match paths::targets(shep_home) {
        Ok(found) => found,
        // The deploy directory exists but could not be listed. There is
        // nothing to iterate, but silence here reads as success: `report(&[])`
        // is empty, so `on_remove` prints nothing and exits 0 with every
        // sheep under the tree left running. One row naming the directory
        // says something instead.
        Err(err) => {
            return vec![Restored::Unlisted {
                dir: shep_home.join("deploy"),
                why: err.to_string(),
            }];
        }
    };
    let names = found.named;
    // Read once for every target rather than once per target: it costs a
    // SaveRoll round trip, and a removal is not the moment to make N of
    // them. Kept as a `Result` rather than `.unwrap_or_default()`: an empty
    // map here is indistinguishable from "nothing is registered", and
    // `put_back` would report every pre-existing sheep as "no longer
    // registered" - a fabricated cause standing in for whatever
    // `roll::registered` actually failed with.
    let registered = roll::registered(daemon).await;

    let mut results = Vec::new();
    // A target the dog cannot name cannot be restored either, and a sheep
    // may still be running out of it.
    for dir in found.unnamed {
        results.push(Restored::Unlisted {
            why: "its directory's name cannot be a sheep's, so it was never polled and is not \
                  restored. A sheep may still be running from inside it: `shep flock` lists \
                  every sheep and `shep describe <sheep>` its cwd. Rename the directory once \
                  none does, then restore by hand"
                .to_owned(),
            dir,
        });
    }
    // Pre-existing sheep whose restore needs the roll, deferred here rather
    // than reported as they are found: a roll that cannot be read is one
    // failure shared by every one of them, not N separate ones.
    let mut blocked_by_roll = Vec::new();

    for sheep in names {
        let tree = Tree::for_sheep(shep_home, &sheep);
        let state = match State::read_for_restore(&tree.state_file()) {
            Ok(state) => state,
            Err(err) => {
                results.push(Restored::Failed {
                    sheep,
                    why: err.to_string(),
                });
                continue;
            }
        };

        // Nothing to restore means the dog bootstrapped this sheep, so it
        // is left running and TOLD about. Deleting an app because a deploy
        // tool was uninstalled would be much worse than leaving it. This
        // needs no roll at all, so a roll that failed to read does not
        // touch it.
        // Read before the origin fields move out below.
        let never_moved = state.deployed.is_none();
        let origin = state.origin;
        let (Some(cwd), Some(script)) = (state.origin_cwd, state.origin_script) else {
            results.push(Restored::LeftRunning {
                sheep,
                from: tree.current(),
            });
            continue;
        };

        let Ok(registered) = &registered else {
            blocked_by_roll.push(sheep);
            continue;
        };

        // An origin recorded is not the same as a sheep moved. `prepare`
        // writes both origin fields, and only a landed cutover writes
        // `deployed`, so an absent `deployed` means the cutover did not land.
        //
        // It does NOT mean the cutover never ran, which is why the shepherd is
        // asked as well. `cut_over` writes `deployed` only on the fully
        // verified path; its `NotVerified` and `Failed` arms return without
        // touching it, having already run `undo_start`, whose own repair can
        // fail and leave a newcomer registered. Reading `deployed` alone
        // called that sheep untouched and reported it as running where it
        // belongs, at the one moment this module exists to catch exactly that.
        //
        // `cwd` is the signal because `cut_over` is the only thing that ever
        // sets it to the tree's `current`, so the shepherd's own registration
        // says whether the cutover got as far as registering.
        if never_moved && !registered_against(registered, &sheep, &tree) {
            results.push(Restored::NeverMoved { sheep, at: cwd });
            continue;
        }

        // The report names where the sheep went: the origin's own cwd when
        // the record carries one, the legacy field otherwise, which is the
        // same choice `put_back` makes.
        let to = origin
            .as_ref()
            .and_then(|origin| origin.cwd.as_deref())
            .map_or_else(|| cwd.clone(), PathBuf::from);
        let outcome = put_back(daemon, &sheep, registered, origin.as_ref(), &cwd, &script).await;
        // A sheep that is back at its origin has no use for its tree, so it
        // lets go of the clone it shared. Only then: every other outcome
        // leaves the sheep running from, or retrying into, the tree.
        let clone_kept = matches!(outcome, PutBack::Done)
            .then(|| retention::release_clone(shep_home, &tree))
            .and_then(Result::err)
            .map(|err| Restored::CloneKept {
                sheep: sheep.clone(),
                why: err.to_string(),
            });
        results.push(
            match outcome {
                PutBack::Done => Restored::Returned { sheep, to },
                PutBack::Untouched(err) => Restored::Failed {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::Reset(err) => Restored::Reset {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::PartlyDeleted(err) => Restored::PartlyDeleted {
                    sheep,
                    why: err.to_string(),
                },
                PutBack::Deleted(err) => Restored::Lost {
                    sheep,
                    why: err.to_string(),
                },
            },
        );
        results.extend(clone_kept);
    }

    if let Err(err) = &registered
        && !blocked_by_roll.is_empty()
    {
        results.push(Restored::RollUnreadable {
            sheep: blocked_by_roll,
            why: err.to_string(),
        });
    }

    results
}

/// The report shep's hook pipes to the operator, which is the whole of what
/// they see about this.
#[must_use]
pub fn report(results: &[Restored]) -> String {
    if results.is_empty() {
        // Silence here is indistinguishable from success. There were no
        // deploy targets at all, and the operator is entitled to be told
        // that rather than left to guess why nothing printed.
        return "no deploy targets, nothing to restore\n".to_owned();
    }
    results
        .iter()
        .map(|result| match result {
            Restored::Returned { sheep, to } => {
                format!("{sheep} restored to {}\n", to.display())
            }
            // Rin's condition for accepting the leave-running case at all.
            // Without this line, "left running, unchanged" is
            // indistinguishable from "quietly abandoned somewhere you will
            // not think to look", which is the failure this whole module
            // exists to prevent.
            Restored::NeverMoved { sheep, at } => {
                format!(
                    "{sheep} was never moved - its cutover did not land - and is still running \
                     from {}\n",
                    at.display()
                )
            }
            Restored::LeftRunning { sheep, from } => {
                format!("{sheep} still running from {}\n", from.display())
            }
            Restored::Failed { sheep, why } => {
                format!("{sheep} could not be restored ({why}); nothing was changed by this dog\n")
            }
            Restored::Unlisted { dir, why } => format!(
                "{} could not be listed ({why}); nothing under it was changed by this dog\n",
                crate::shared::printable(dir.display())
            ),
            // NOT the same wording as `Failed`: this sheep was stopped and
            // a fresh instance started to get its own previous config back,
            // so nothing mid-flight survived even though the config an
            // operator reads now matches what it was a moment ago.
            Restored::Reset { sheep, why } => format!(
                "{sheep} could not be restored ({why}), so its previous configuration was put \
                 back instead - doing that stopped it and started it again, so it is running \
                 with the same config as before but nothing mid-flight survived\n"
            ),
            // Neither "left as it is" nor "gone from the flock" is true
            // here, so this gets wording that says exactly that, and sends
            // the operator to the one place that can say what is really
            // running: `shep describe`.
            Restored::PartlyDeleted { sheep, why } => format!(
                "{sheep}: only SOME of its instances could be stopped before a delete failed \
                 ({why}), so it is neither fully running nor fully removed. Run `shep describe \
                 {sheep}` to see what is actually still there, and `shep flock` for whether it \
                 is still registered, before assuming either.\n"
            ),
            // The row an operator must not misread. Every other outcome
            // leaves a running app; this one does not, and "could not be
            // restored" would have them assume it did.
            Restored::Lost { sheep, why } => format!(
                "{sheep} IS NO LONGER REGISTERED: restoring it failed ({why}) and so did \
                 putting its previous configuration back, so it is stopped and gone from the \
                 flock. It will not come back on its own after a restart. Re-register it from \
                 its own Flockfile.\n"
            ),
            Restored::CloneKept { sheep, why } => format!(
                "{sheep}: the clone it shared with other sheep could not be let go of ({why}); \
                 remove it by hand from $SHEP_HOME/deploy/repos once no sheep needs it\n"
            ),
            // One row naming every affected sheep and the roll's own
            // cause, rather than one wrong-shaped guess per sheep.
            Restored::RollUnreadable { sheep, why } => format!(
                "{}: none of these could be checked against the muster roll, so none of them \
                 could be restored ({why})\n",
                sheep.join(", ")
            ),
        })
        .collect()
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
