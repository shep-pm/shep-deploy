//! What the loop says about each target, and how often it says it.
//!
//! Also holds [`CatchUnwind`], which turns a panic in a deploy into the
//! `Err` that is reported here like any other failure.

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::deploy::Outcome;
use crate::error::Error;

/// A future that turns a panic in the one it wraps into an `Err`, so the
/// loop around it keeps going.
///
/// Written here rather than taken from a crate because it is a dozen lines:
/// each poll is run under [`catch_unwind`], and a payload that comes out is
/// the future's answer. The inner future is pinned on the heap by the
/// caller, which is what makes it `Unpin` and lets it be polled through a
/// plain closure.
pub(super) struct CatchUnwind<F>(pub(super) Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().0;
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// The message a panic carried, or a note that it carried none.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

/// What one target's outcome is worth saying, and which log it belongs in.
///
/// Split from the writing so that both halves can be checked: what a tick
/// says is the only observable this module has, and a loop whose prints
/// were all deleted passed the suite that came before this type existed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Said {
    /// Something that happened, for stdout.
    Note(String),
    /// Something that did not, for stderr.
    Complaint(String),
}

impl Said {
    /// The line itself, without the stream it goes to.
    pub(super) fn text(&self) -> &str {
        match self {
            Self::Note(text) | Self::Complaint(text) => text,
        }
    }
}

/// What to say about one target's outcome, or `None` for silence.
///
/// `UpToDate` says nothing at all, deliberately: it is the answer to almost
/// every tick of almost every target, and a dog that logged a line per
/// target per thirty seconds would bury the deploys nobody wants to miss
/// under its own heartbeat.
///
/// A rollback is a complaint even though it arrives as an `Ok`. The machine
/// is healthy, which is why it is not an error - and the deploy did not
/// land, which is why it does not belong in the log an operator reads to
/// see what shipped.
pub(super) fn report(sheep: &str, outcome: &Result<Outcome, Error>) -> Option<Said> {
    // Every key is printed as the line's label, and one kind of key is a
    // directory name that exists precisely because it carries something a
    // log line cannot.
    let sheep = &crate::shared::printable(sheep);
    match outcome {
        Ok(Outcome::UpToDate) => None,
        Ok(Outcome::Deployed { sha }) => Some(Said::Note(format!("{sheep} deployed {sha}"))),
        Ok(Outcome::Skipped { sha }) => Some(Said::Note(format!(
            "{sheep} skipped {sha}, nothing under its reload paths changed"
        ))),
        Ok(Outcome::RolledBack { to, why }) => Some(Said::Complaint(format!(
            "{sheep} rolled back to {to}: {why}"
        ))),
        Err(err) => Some(Said::Complaint(format!("{sheep}: {err}"))),
    }
}

/// How many ticks a target's repeated line is muted for before it is said
/// again.
///
/// Counted in ticks rather than in time, so a loop that polls less often
/// says it less often - 120 is an hour at the default interval. It is not
/// zero, because the alternative is a condition that is mentioned once and
/// then never again: a dog running for months would have its only
/// explanation in a log that has since rotated, and a target removed and
/// recreated would have its first refusal swallowed by the entry left over
/// from its predecessor.
const RESAY: u32 = 120;

/// One target's last line, and how many ticks it has been muted for.
pub(super) struct Repeat {
    /// The line as it was said.
    line: String,
    /// Ticks since it was last said.
    muted: u32,
}

/// Whether `line` is worth saying, given what was last said about this
/// target.
///
/// A line that differs from the last one is always worth saying, and a
/// repeat of the same line is not, until [`RESAY`] ticks have gone by.
///
/// The criterion is the line rather than a list of error variants, and that
/// is the whole of the fix it replaces. "The refusals that cannot clear on
/// their own" is not a set anybody can enumerate correctly - a tree the
/// cutover never landed on, a `deploy.toml` that will not parse, a deploy
/// directory that cannot be listed, a remote that no longer resolves and a
/// build that fails on a committed typo are all permanent until somebody
/// acts - and an enumeration that misses one prints 2,880 identical lines a
/// day. A condition that really can clear on its own says something
/// different the moment it does, which re-arms the target without anything
/// having to know which conditions those are.
///
/// Answers how many repeats were swallowed since the line was last said,
/// so a line said again after [`RESAY`] ticks can carry the count: an
/// operator reading it an hour later can then tell "still broken" from
/// "broke again, many times". `None` is silence; `Some(0)` is a line that
/// is new.
pub(super) fn worth_saying(
    previous: &mut BTreeMap<String, Repeat>,
    sheep: &str,
    line: &str,
) -> Option<u32> {
    let mut swallowed = 0;
    if let Some(seen) = previous.get_mut(sheep)
        && seen.line == line
    {
        seen.muted += 1;
        if seen.muted < RESAY {
            return None;
        }
        // The occurrence being said now is not one that was swallowed.
        swallowed = seen.muted - 1;
    }

    previous.insert(
        sheep.to_owned(),
        Repeat {
            line: line.to_owned(),
            muted: 0,
        },
    );
    Some(swallowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a panic inside a deploy stops being caught. The loop's one
    /// promise is that a target's failure does not stop the others, and a
    /// panic is the failure `Result` cannot carry.
    #[tokio::test]
    async fn a_panic_becomes_an_err_rather_than_ending_the_loop() {
        let caught = CatchUnwind(Box::pin(async {
            tokio::task::yield_now().await;
            panic!("the deploy fell over");
        }))
        .await;
        let payload = caught.expect_err("the panic must be caught");
        assert_eq!(panic_message(payload.as_ref()), "the deploy fell over");

        let fine = CatchUnwind(Box::pin(async { 7 })).await;
        assert_eq!(fine.ok(), Some(7));
    }

    /// fails if a panic's message is lost on the way to the row. A `String`
    /// payload is what `panic!` with a format string gives, a `&str` is what
    /// a literal gives, and anything else has to say it had no message
    /// rather than print nothing.
    #[test]
    fn a_panic_message_is_read_from_either_payload_shape() {
        assert_eq!(panic_message(&"literal"), "literal");
        assert_eq!(panic_message(&String::from("formatted")), "formatted");
        assert_eq!(panic_message(&42_u8), "a panic with no message");
    }

    /// fails if a target that keeps failing the same way keeps saying so.
    /// At thirty seconds, a line an interval is 2,880 identical lines a day
    /// with everything the dog really has to say buried in between them.
    #[test]
    fn a_line_that_repeats_is_said_once() {
        let mut previous = BTreeMap::new();

        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_some());
        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_none());
        assert!(worth_saying(&mut previous, "web", "web: the remote is gone").is_none());
        // A different target is a different line.
        assert!(worth_saying(&mut previous, "koji", "koji: the remote is gone").is_some());
    }

    /// fails if the mute is keyed on anything narrower than the line. The
    /// criterion it replaced was one error variant, and every other
    /// permanent condition - an unparseable record, a deploy directory that
    /// cannot be listed, a remote that no longer resolves, a build that
    /// fails on a committed typo - went on saying so every interval. There
    /// is no correct list of those, which is the argument for the line.
    #[test]
    fn every_repeated_line_is_muted_not_one_chosen_kind() {
        let mut previous = BTreeMap::new();
        let lines = [
            "web: bad deploy configuration: /srv/deploy/web/deploy.toml: expected an equals",
            "web: `git fetch origin` exited with status 128: could not resolve host",
            "web: the build command exited with status 1",
        ];

        for line in lines {
            assert!(worth_saying(&mut previous, "web", line).is_some(), "{line}");
            assert!(worth_saying(&mut previous, "web", line).is_none(), "{line}");
        }
    }

    /// fails if a condition that cleared stays muted, so that its coming
    /// back is never mentioned. A line that differs is new information, and
    /// a target that has started working again is the clearest case of it.
    #[test]
    fn a_line_that_changes_is_said_again() {
        let mut previous = BTreeMap::new();
        let broken = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", broken).is_some());
        assert!(worth_saying(&mut previous, "web", broken).is_none());
        assert!(worth_saying(&mut previous, "web", "web deployed abc1234").is_some());
        assert!(worth_saying(&mut previous, "web", broken).is_some());
    }

    /// fails if the count a re-said line carries is off by one. The
    /// occurrence that lifts the mute is the one being printed, so it is
    /// not among the swallowed; `RESAY` occurrences after the first say,
    /// `RESAY - 1` were swallowed.
    #[test]
    fn a_line_said_again_counts_only_what_was_swallowed() {
        let mut previous = BTreeMap::new();
        let line = "web: the remote is gone";

        assert_eq!(worth_saying(&mut previous, "web", line), Some(0), "new");
        for _ in 1..RESAY {
            assert_eq!(worth_saying(&mut previous, "web", line), None);
        }
        assert_eq!(
            worth_saying(&mut previous, "web", line),
            Some(RESAY - 1),
            "the one being said is not swallowed"
        );
    }

    /// fails if a muted line is muted forever. A dog runs for months, and a
    /// condition mentioned once in a log that has since rotated is a
    /// condition nobody can find. Said again every `RESAY` ticks, which is
    /// an hour at the default interval.
    #[test]
    fn a_muted_line_is_said_again_eventually() {
        let mut previous = BTreeMap::new();
        let line = "web: the remote is gone";

        assert!(worth_saying(&mut previous, "web", line).is_some());
        let said = (0..RESAY * 2)
            .filter(|_| worth_saying(&mut previous, "web", line).is_some())
            .count();

        assert_eq!(said, 2, "once every RESAY ticks, not once ever");
    }

    /// fails if what a tick did stops reaching the log, or reaches the
    /// wrong one. This is the whole of what an operator sees of the poll
    /// loop: nothing else about it is observable at all.
    #[test]
    fn each_outcome_says_what_it_did_and_where() {
        assert_eq!(report("web", &Ok(Outcome::UpToDate)), None, "the heartbeat");
        assert_eq!(
            report(
                "web",
                &Ok(Outcome::Deployed {
                    sha: "a1b2c3".to_owned()
                })
            ),
            Some(Said::Note("web deployed a1b2c3".to_owned()))
        );
        // A rollback arrives as an `Ok` because the machine is healthy, and
        // is still a complaint because the deploy did not land.
        assert_eq!(
            report(
                "web",
                &Ok(Outcome::RolledBack {
                    to: "old".to_owned(),
                    why: "it did not come up".to_owned()
                })
            ),
            Some(Said::Complaint(
                "web rolled back to old: it did not come up".to_owned()
            ))
        );
        let err = report("web", &Err(Error::Build { status: Some(1) }));
        let Some(Said::Complaint(text)) = err else {
            panic!("a failure is a complaint: {err:?}");
        };
        assert!(text.starts_with("web: "), "{text}");
    }
}
