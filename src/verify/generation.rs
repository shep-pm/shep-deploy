//! The pids serving a sheep, captured so a reload can be told from what was
//! already there.

use std::collections::BTreeSet;

use shep_client::shep_core::protocol::ProcessInfo;

use crate::daemon::Daemon;
use crate::error::Error;

/// The pids serving one sheep at a moment in time.
///
/// Captured before a reload so that [`super::wait`] can tell the replacement from
/// the process that was already there. See the module doc for why this
/// exists at all, and why it is pids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generation {
    /// The pids that had a process.
    pids: BTreeSet<u32>,
    /// Every row the shepherd listed, with or without a pid.
    ///
    /// Kept apart from `pids` because they can differ, and the difference
    /// used to fail healthy deploys: a replica in `WaitingRestart` has no
    /// pid, so it was missing from `pids` and present in the listing, and
    /// [`Self::has_turned_over`] compared the pid count against the row
    /// count and never found them equal.
    rows: u32,
}

impl Generation {
    /// The generation serving `sheep` right now.
    ///
    /// A sheep the shepherd has never heard of, or one with no running
    /// instance, gives an empty generation rather than an error: nothing was
    /// serving, so anything that comes up is new.
    ///
    /// # Errors
    /// Whatever [`Daemon::describe`] returns.
    pub async fn of<D: Daemon>(daemon: &D, sheep: &str) -> Result<Self, Error> {
        let flock = daemon.describe(sheep).await?;
        Ok(Self::of_infos(&flock.iter().collect::<Vec<_>>()))
    }

    /// How many instances this generation has.
    ///
    /// Measured, which is the point: `AppConfig::instances` is the count the
    /// release's Flockfile ASKS for, and `shep stock <sheep> <n>` changes
    /// what is actually running without touching that file. A reload costs
    /// one swap per instance the shepherd lists, so this is the number
    /// `crate::deploy`'s budget has to multiply by, and the number a
    /// turnover has to come back at.
    #[must_use]
    pub fn instances(&self) -> u32 {
        self.rows
    }

    /// The generation a listing already in hand describes.
    ///
    /// [`Self::of`] takes a [`Daemon`] and issues a request; this takes the
    /// answer to one. The cutover's first phase already holds the listing it
    /// wants to freeze, and asking again would both cost a round trip and
    /// risk freezing a DIFFERENT set of pids than the one it just decided
    /// on, which is the sort of gap a crash-looping release fits through.
    ///
    /// `crate::optin`'s cutover uses it twice for that reason: once on the
    /// listing it reads before the `Start`, whose ids it also keeps, so the
    /// ids to delete and the pids to compare against describe one instant
    /// rather than two.
    pub(crate) fn of_infos(infos: &[&ProcessInfo]) -> Self {
        Self {
            pids: infos.iter().filter_map(|info| info.pid).collect(),
            // A flock of more than u32::MAX processes is not a thing this
            // host could be running, but saturating says so without a cast
            // that could wrap.
            rows: u32::try_from(infos.len()).unwrap_or(u32::MAX),
        }
    }

    /// Whether `info` is running under a pid this generation never had.
    ///
    /// An instance with no pid at all is never new. That is the honest
    /// reading rather than a defensive one: a `WaitingRestart` or errored
    /// instance has no process, and a deploy verified against one would be
    /// verified against nothing.
    ///
    /// Two callers, asking the same question for different reasons. [`super::wait`]
    /// asks it inside [`Self::has_turned_over`], which additionally requires
    /// EVERY instance to be new - a reload replaces them all, so an old one
    /// still present means the reload is not finished. `crate::optin`'s
    /// cutover asks it bare, because a cutover deliberately leaves the old
    /// instance running beside the new one and a full turnover would never
    /// arrive. Do not "fix" either call site by tightening it toward the
    /// other: they are different requests to the shepherd, not two spellings
    /// of one.
    pub(crate) fn is_new(&self, info: &ProcessInfo) -> bool {
        info.pid.is_some_and(|pid| !self.pids.contains(&pid))
    }

    /// Whether `info` is one of this generation's own instances.
    ///
    /// The mirror of [`Self::is_new`], for the dwell: after a turnover the
    /// question stops being "is this different" and becomes "is this still
    /// the same one". `crate::optin`'s cutover dwells on the same question
    /// about the instance its own `Start` spawned.
    pub(crate) fn holds(&self, info: &ProcessInfo) -> bool {
        info.pid.is_some_and(|pid| self.pids.contains(&pid))
    }

    /// Whether `flock` is entirely made of instances this generation never
    /// had, in a state `accept` is happy with.
    ///
    /// Empty is never a turnover: a sheep the shepherd cannot find has not
    /// been verified.
    pub(super) fn has_turned_over(
        &self,
        flock: &[ProcessInfo],
        accept: fn(&ProcessInfo) -> bool,
    ) -> bool {
        // The count as well, measured against what was running BEFORE. A
        // two-instance sheep that loses a replica during the reload lists one
        // new instance, and one new instance is "all new": the turnover was
        // accepted, `settled` became that single pid, and every later check
        // compared the survivor against itself. A sheep that came back at half
        // strength verified.
        //
        // Checked here rather than only at the dwell, though the dwell is
        // where it was found: this is where `settled` is frozen, and both
        // modes then re-check the count at their dwell against
        // `settled.instances()`.
        //
        // Skipped when nothing was running before, because then there is no
        // count to hold to and `!flock.is_empty()` is the whole of what can be
        // asked.
        // Rows against rows. `instances()` counts every row the shepherd
        // listed before the reload, pid or no pid, so a replica that was
        // between restarts at capture time still counts as one the reload
        // has to bring back. Counting pids instead undercounted by exactly
        // those, and a flock that came back whole could never match.
        let count_holds = self.instances() == 0
            || u32::try_from(flock.len()).is_ok_and(|listed| listed == self.instances());

        !flock.is_empty()
            && count_holds
            && flock.iter().all(|info| self.is_new(info) && accept(info))
    }
}

#[cfg(test)]
mod tests {
    use shep_client::shep_core::status::ProcStatus;

    use super::*;
    use crate::verify::test_support::{Listings, instance};

    /// fails if capturing the pre-reload generation loses a pid, or invents
    /// one for an instance that has none. Everything above rests on this
    /// set being what was actually serving.
    #[tokio::test(start_paused = true)]
    async fn a_generation_is_the_pids_that_are_serving() {
        let daemon = Listings::new(vec![vec![
            instance(1, ProcStatus::Online, 12835),
            instance(2, ProcStatus::Online, 12836),
            ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart).build(),
        ]]);
        let generation = Generation::of(&daemon, "web").await.unwrap();
        assert_eq!(
            generation,
            Generation {
                pids: [12835, 12836].into_iter().collect(),
                rows: 3,
            }
        );
    }
}
