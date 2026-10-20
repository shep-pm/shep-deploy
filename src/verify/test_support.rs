//! What the `verify` tests share: a fake shepherd that answers with scripted
//! listings, and the instances and generations to script it with.

use std::cell::Cell;
use std::time::Duration;

use shep_client::RequestError;
use shep_client::shep_core::protocol::ProcessInfo;
use shep_client::shep_core::status::ProcStatus;

use super::Generation;
use crate::daemon::Daemon;
use crate::error::Error;

/// One instance, with the id and pid the tests care about.
pub(super) fn instance(id: u32, status: ProcStatus, pid: u32) -> ProcessInfo {
    ProcessInfo::builder(id, "web", status)
        .pid(Some(pid))
        .build()
}

/// The generation a reload would be replacing: one instance at `pid`.
pub(super) fn serving(pid: u32) -> Generation {
    serving_all(&[pid])
}

/// The generation a reload would be replacing: one instance per pid.
pub(super) fn serving_all(pids: &[u32]) -> Generation {
    let infos: Vec<ProcessInfo> = pids
        .iter()
        .map(|&pid| instance(0, ProcStatus::Online, pid))
        .collect();
    Generation::of_infos(&infos.iter().collect::<Vec<_>>())
}

/// A [`Daemon`] whose `describe` walks through a fixed sequence of whole
/// flock listings, one per call, repeating the last once the sequence is
/// exhausted rather than panicking or returning an empty `Vec`. `Probed`
/// calls `describe` many times inside one wait, and these tests only
/// need to name the interesting moments.
///
/// Whole listings rather than statuses: what `wait` judges is the shape
/// of the flock - which pids are in it, not only what state one of them
/// is in - so a fake that could only vary the status could not express
/// the failure this module exists to catch.
pub(super) struct Listings {
    sequence: Vec<Vec<ProcessInfo>>,
    next: Cell<usize>,
}

impl Listings {
    pub(super) fn new(sequence: Vec<Vec<ProcessInfo>>) -> Self {
        Self {
            sequence,
            next: Cell::new(0),
        }
    }
}

impl Daemon for Listings {
    async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        let Some(last) = self.sequence.len().checked_sub(1) else {
            return Ok(Vec::new());
        };
        let index = self.next.get().min(last);
        self.next.set((index + 1).min(last));
        Ok(self.sequence[index].clone())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, reload, restart, save_roll, set_smit,
    );
}

/// A daemon that fails its first `describe` with a retryable error, then
/// answers with a flock that has turned over.
pub(super) struct Blips {
    pub(super) calls: Cell<u32>,
    pub(super) after: Vec<ProcessInfo>,
}

impl Daemon for Blips {
    async fn describe(&self, _sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        let n = self.calls.get();
        self.calls.set(n + 1);
        if n == 0 {
            return Err(Error::Request(RequestError::Timeout {
                after: Duration::from_secs(1),
            }));
        }
        Ok(self.after.clone())
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, reload, restart, save_roll, set_smit,
    );
}
