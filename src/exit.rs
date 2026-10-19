//! The exit status a run ends on.
//!
//! This dog shares shep's exit-code taxonomy for the causes they have in
//! common and adds two numbers of its own, [`ROLLED_BACK`] and [`STRANDED`].
//! Kept apart from `main` so the mapping is testable and is read in one place.

use crate::deploy::Outcome;
use crate::error::Error;

/// The exit code for a deploy that was rolled back.
///
/// shep's own taxonomy (`docs/specs/shep-v1.md` section 9) runs from 0 to 11
/// and this is the next free number, claimed rather than invented: a
/// rollback is a cause shep has no code for, and every cause this dog
/// shares with shep uses shep's number for it. A script must be able to
/// tell three outcomes apart, deployed, cleanly reverted, and broke, and
/// two of those were the same code until Rin ruled otherwise.
const ROLLED_BACK: u8 = 12;

/// A cutover that landed and then could not tidy up: the sheep is live on the
/// new release, and something after the swap failed.
///
/// Its own code rather than the generic 1, because a script has to tell three
/// outcomes apart: it worked, it worked and needs tidying, it failed. The
/// poll loop is why that matters. Unattended, a generic failure here reads as
/// a deploy that did not land, and it would retry one that did.
const STRANDED: u8 = 13;

/// The exit code for a run that finished, reporting what it finished as.
pub const fn code_for_outcome(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::UpToDate | Outcome::Deployed { .. } => 0,
        Outcome::RolledBack { .. } => ROLLED_BACK,
    }
}

/// The exit code for a run that failed.
///
/// The first two arms are this dog's own numbers, for outcomes shep has no
/// code for. Every arm after them is shep's own number for the same cause, so
/// an operator reading a dog's status does not have to learn a second
/// vocabulary. Anything with no more specific cause is 1, which is shep's
/// rule as well.
pub fn code_for(err: &Error) -> u8 {
    match err {
        Error::RolledBack { .. } => ROLLED_BACK,
        Error::Stranded { .. } => STRANDED,
        // The same number as any other configuration refusal: it is one,
        // from outside. What its own variant buys is inside - see the
        // variant's doc, and `crate::poll::worth_saying`.
        Error::Config(_) | Error::NotCutOver { .. } => 4,
        // A refused handshake takes the same number as a socket that could
        // not be reached, because it is the same thing from an operator's
        // side: this dog and the shepherd are not talking. The message says
        // which of the two it was, and that is where the difference belongs.
        Error::Connect(_) | Error::Refused { .. } => 5,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// fails if a rolled-back deploy stops being distinguishable from a
    /// hard failure by exit status alone. A script running
    /// `shep deploy web && notify` has three outcomes to tell apart:
    /// deployed, rejected and cleanly reverted, and broke. Collapsing the
    /// middle one into either neighbour is what makes "reports a success it
    /// did not achieve" possible, which is the species of every serious
    /// finding in the engine plan.
    #[test]
    fn a_rollback_has_its_own_code_distinct_from_failure_and_success() {
        let rolled_back = Error::RolledBack {
            to: "old-sha".to_owned(),
            source: Box::new(Error::Build { status: Some(1) }),
        };
        assert_eq!(code_for(&rolled_back), ROLLED_BACK);
        assert_ne!(ROLLED_BACK, 0);
        assert_ne!(
            code_for(&rolled_back),
            code_for(&Error::Build { status: Some(1) })
        );
    }

    /// fails if a cutover that landed and then could not tidy up is reported
    /// as an ordinary failure. The sheep IS live on the new release; only the
    /// cleanup after the swap did not finish. The poll loop is why the
    /// distinction has to survive into the exit status: unattended, a generic
    /// failure here reads as a deploy that never landed, and it would redeploy
    /// one that did.
    #[test]
    fn a_stranded_cutover_is_neither_a_success_nor_an_ordinary_failure() {
        let stranded = Error::Stranded {
            sheep: "web".to_owned(),
            sha: "abc1234".to_owned(),
            ids: vec![3],
        };
        assert_eq!(code_for(&stranded), STRANDED);
        assert_ne!(STRANDED, 0);
        assert_ne!(
            code_for(&stranded),
            code_for(&Error::Build { status: Some(1) })
        );
        assert_ne!(code_for(&stranded), ROLLED_BACK);
    }

    /// fails if this dog stops joining shep's own exit-code taxonomy and
    /// starts inventing numbers. These four are shep's, from
    /// docs/specs/shep-v1.md section 9, and an operator who has learned that
    /// 5 means "no daemon answered" should not have to learn a second
    /// meaning for it because a dog chose differently.
    #[test]
    fn the_shared_causes_use_sheps_own_numbers() {
        assert_eq!(code_for(&Error::Config("bad".to_owned())), 4);
        assert_eq!(code_for(&Error::Protocol("odd".to_owned())), 1);
        assert_eq!(
            code_for(&Error::Git {
                command: "git fetch".to_owned(),
                status: Some(128),
                stderr: String::new(),
            }),
            1
        );
        assert_eq!(code_for(&Error::Build { status: Some(3) }), 1);
        assert_eq!(
            code_for(&Error::Connect(shep_client::ConnectError::HandshakeClosed)),
            5
        );
        // A tree the cutover never landed on is a configuration problem
        // like any other from outside, whatever the loop makes of it
        // inside: giving it a variant of its own must not give it a number
        // of its own.
        assert_eq!(
            code_for(&Error::NotCutOver {
                sheep: "web".to_owned(),
                tree: PathBuf::from("/srv/shep/deploy/web"),
            }),
            4
        );
    }

    /// fails if a rollback that happened on the ORDINARY path stops being
    /// reported. `Outcome::RolledBack` is the common trigger, a verify that
    /// timed out, and `Error::RolledBack` is the rarer one where something
    /// failed after the reload. Both mean the requested deploy did not
    /// happen, so both take the same code; only this one is reached through
    /// `Ok`.
    #[test]
    fn the_ok_rollback_path_reports_the_same_code() {
        let outcome = Outcome::RolledBack {
            to: "old-sha".to_owned(),
            why: "it did not come up".to_owned(),
        };
        assert_eq!(code_for_outcome(&outcome), ROLLED_BACK);
        assert_eq!(code_for_outcome(&Outcome::UpToDate), 0);
        assert_eq!(
            code_for_outcome(&Outcome::Deployed {
                sha: "new".to_owned()
            }),
            0
        );
    }

    /// fails if a refused handshake stops being an error at all, or starts
    /// taking a number that means something else. It is 5 for the same
    /// reason a dead socket is: this dog and the shepherd are not talking.
    #[test]
    fn a_refused_handshake_exits_on_the_no_daemon_code() {
        let refused = Error::Refused {
            daemon_version: Some("0.4.0".to_owned()),
            message: "protocol mismatch".to_owned(),
        };
        assert_eq!(code_for(&refused), 5);
        assert_eq!(
            code_for(&refused),
            code_for(&Error::Connect(shep_client::ConnectError::HandshakeClosed))
        );
    }
}
