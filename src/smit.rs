//! The short string this dog attaches to a sheep, which `shep flock` paints
//! without understanding it.
//!
//! ```text
//! ▲ main@a1b2c3      watched
//! ⏸ main@a1b2c3      manual
//! ⏸ main@a1b2c3 +d4e5f6  manual, with d4e5f6 staged
//! ```
//!
//! The two marks differ so `shep flock` answers "which of these is actually
//! being watched" without a second command, which was the requirement
//! smits were designed for.
//!
//! # Ephemeral, and that is a lifecycle decision
//!
//! The daemon holds smits in memory and never persists them. When a dog
//! stops for any reason, disabled, rehomed, crashed, or lost to a daemon
//! restart, its smits go with it and it republishes on its next poll. The
//! alternative, persisting them, leaves `shep flock` showing a mark
//! attributed to a dog that no longer exists, and removes that orphan class
//! only by adding cleanup to every path that can stop a dog.
//!
//! # Width is shep's half, not this one
//!
//! The smit may be dropped on a narrow terminal, and may never be dropped
//! at a full-width one, because being seen regularly at full width is what
//! makes dropping it acceptable at all. That is a property of `shep flock`'s
//! adaptive column dropping and is pinned there, not here: this module has
//! no terminal and no table.

use shep_client::shep_core::protocol::Smit;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::state::{State, Watch};

/// How many characters of a sha the smit shows.
///
/// Six, matching what git itself abbreviates to by default at this
/// repository size, and short enough that the whole smit stays inside a
/// column that has to compete with nine others.
const ABBREVIATED: usize = 6;

/// `get`, not a slice: a hand-edited deploy.toml carrying a short sha must
/// degrade to a shorter smit, never take the dog down on its next poll.
fn abbreviate(sha: &str) -> &str {
    sha.get(..ABBREVIATED).unwrap_or(sha)
}

/// The smit for one target.
///
/// `none` rather than an empty sha for a target between opt-in and its
/// first verified deploy: `main@` reads as deployed at nothing, where
/// `main@none` reads as not deployed yet.
///
/// A staged sha follows the deployed one as ` +d4e5f6`, on a manual target
/// only. A branch too long for the suffix is shortened so the suffix stays.
#[must_use]
pub fn text(state: &State) -> String {
    let mark = match state.watch {
        Watch::Auto => '▲',
        Watch::Manual => '⏸',
    };
    let sha = state.deployed.as_deref().map_or("none", abbreviate);
    // A watched target deploys its head on the next tick, so a sha waiting
    // on a deploy "being asked for" only means something on a manual one.
    match (state.watch, state.staged.as_deref()) {
        (Watch::Manual, Some(staged)) => {
            // `fit` cuts from the end, which would drop the staged sha, the
            // one thing this suffix is there to show. Shorten the branch so
            // the suffix always fits.
            let suffix = format!("@{sha} +{}", abbreviate(staged));
            let room = Smit::MAX_CHARS.saturating_sub(2 + suffix.chars().count());
            let branch: String = state.branch.chars().take(room).collect();
            format!("{mark} {branch}{suffix}")
        }
        _ => format!("{mark} {}@{sha}", state.branch),
    }
}

/// Shorten `text` to [`Smit::MAX_CHARS`] and drop any control character, so
/// what is published is something the shepherd's own
/// [`Smit::from_str`](core::str::FromStr::from_str) accepts.
///
/// A branch name is the only variable-length part of a smit, and a branch
/// name long enough to matter is a real thing an operator can name, not a
/// mistake to refuse over. Publishing is cosmetic: showing a shortened mark
/// beats showing nothing at all because the daemon would have refused the
/// full one.
///
/// Control characters get the same treatment for the same reason. git
/// refuses them in a ref name, so nothing this crate writes produces one,
/// but `deploy.toml` is hand-edited and a smit the daemon refuses is
/// refused on every tick for as long as the record says so.
fn fit(text: String) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(Smit::MAX_CHARS)
        .collect()
}

/// Publish `sheep`'s smit, built from `state`, to the shepherd.
///
/// Called every tick for every target, and above the `due` check on
/// purpose: a manual target's smit is exactly how an operator sees that it
/// is paused, so skipping it would hide the state the smit exists to show.
///
/// Republished every tick rather than on change, because the daemon holds
/// smits in memory and drops them whenever this dog stops. A
/// publish-on-change dog would show nothing after a daemon restart until
/// its next deploy, which for a healthy target could be weeks.
///
/// # Errors
/// As [`Daemon::set_smit`]. The caller is expected to report a failure and
/// otherwise ignore it: a smit is cosmetic, and a daemon that refuses one
/// is a daemon this dog can still deploy through. The caller reports it as
/// a row of its own, keyed with a `/` that no sheep name can carry, so it
/// reaches the mute: a daemon that refuses every smit would otherwise say
/// so once per target per tick, forever.
pub async fn publish<D: Daemon>(daemon: &D, sheep: &str, state: &State) -> Result<(), Error> {
    daemon.set_smit(sheep, &fit(text(state))).await
}

/// Publish `sheep`'s smit again, but only if `state` renders differently
/// from `painted`, the [`text`] last painted for it.
///
/// For a loop that has just changed the state it painted from, so `shep
/// flock` is true as soon as the loop knows, not a tick later. It sends
/// nothing when nothing changed, so a target that did not deploy costs no
/// request.
///
/// # Errors
/// As [`publish`].
pub async fn repaint<D: Daemon>(
    daemon: &D,
    sheep: &str,
    painted: &str,
    state: &State,
) -> Result<(), Error> {
    if text(state) == painted {
        return Ok(());
    }
    publish(daemon, sheep, state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(watch: Watch, deployed: Option<&str>) -> State {
        State {
            deployed: deployed.map(str::to_owned),
            watch,
            ..crate::fixtures::state()
        }
    }

    /// fails if a watched and a manual target render the same. The smit
    /// exists so `shep flock` answers "which of these is actually being
    /// watched" without a second command, and two targets that look
    /// identical answer nothing.
    #[test]
    fn watched_and_manual_are_told_apart_at_a_glance() {
        assert_eq!(
            text(&target(Watch::Auto, Some("a1b2c3d4e5f6"))),
            "▲ main@a1b2c3"
        );
        assert_eq!(
            text(&target(Watch::Manual, Some("a1b2c3d4e5f6"))),
            "⏸ main@a1b2c3"
        );
    }

    /// fails if the branch stops being named. It is the fact an operator
    /// most often wants and cannot get anywhere else in `shep flock`: three
    /// sheep deployed from one repository differ by branch, and Rin runs
    /// exactly that arrangement with bpm, ctm and opm.
    #[test]
    fn the_branch_is_named_not_assumed() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.branch = "stable".to_owned();
        assert_eq!(text(&state), "▲ stable@a1b2c3");
    }

    /// fails if a target with no deploy yet renders as a lie. `deployed` is
    /// None between opt-in and the first verified deploy, and printing an
    /// empty sha there would read as "deployed at nothing" rather than
    /// "not deployed yet".
    #[test]
    fn a_target_with_no_deploy_yet_says_so() {
        assert_eq!(text(&target(Watch::Auto, None)), "▲ main@none");
    }

    /// fails if a sha shorter than the abbreviation panics or is padded.
    /// Nothing this crate writes produces one, so this is about a
    /// hand-edited deploy.toml degrading rather than taking the dog down on
    /// its next poll.
    #[test]
    fn a_short_sha_degrades_rather_than_panicking() {
        assert_eq!(text(&target(Watch::Auto, Some("abc"))), "▲ main@abc");
    }

    /// fails if a long branch name produces something `Smit::from_str`
    /// refuses. A branch name is the only variable-length part of a smit,
    /// and this is the case that would otherwise cost a deploy nothing
    /// merely for having a long one.
    #[test]
    fn a_long_branch_name_is_shortened_to_fit() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.branch = "x".repeat(100);
        let shortened = fit(text(&state));
        assert_eq!(shortened.chars().count(), Smit::MAX_CHARS);
        assert!(shortened.parse::<Smit>().is_ok(), "{shortened}");
    }

    /// fails if a staged sha is not shown, or is shown before the deployed
    /// one. The deployed sha is what is serving, and the staged one is what
    /// is waiting, so it reads second.
    #[test]
    fn a_staged_sha_is_shown_after_the_deployed_one() {
        let mut state = target(Watch::Manual, Some("a1b2c3d4e5f6"));
        state.staged = Some("d4e5f6a7b8c9".to_owned());
        assert_eq!(text(&state), "⏸ main@a1b2c3 +d4e5f6");
    }

    /// fails if a watched target shows a staged sha. `--stage` refuses a
    /// watched target, but `--watch auto` after staging leaves the sha in the
    /// record, and the next tick deploys the head anyway.
    #[test]
    fn a_watched_target_does_not_show_a_staged_sha() {
        let mut state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        state.staged = Some("d4e5f6a7b8c9".to_owned());
        assert_eq!(text(&state), "▲ main@a1b2c3");
    }

    /// fails if a long branch costs the staged sha its place. The branch is
    /// the variable part, so it has to lose its own tail, and what is
    /// published must still parse and still end in the deployed and staged
    /// shas.
    #[test]
    fn a_staged_sha_cannot_push_the_smit_past_the_shepherds_limit() {
        let mut state = target(Watch::Manual, Some("a1b2c3d4e5f6"));
        state.branch = "x".repeat(100);
        state.staged = Some("d4e5f6a7b8c9".to_owned());
        let shortened = fit(text(&state));
        assert_eq!(shortened.chars().count(), Smit::MAX_CHARS);
        assert!(shortened.ends_with("@a1b2c3 +d4e5f6"), "{shortened}");
        assert!(shortened.parse::<Smit>().is_ok(), "{shortened}");
    }

    /// fails if `fit` and `Smit::from_str` ever disagree about where the
    /// limit is. Both count `char`s today, so a string of exactly
    /// `MAX_CHARS` is accepted and one char more is refused. `fit` takes
    /// `MAX_CHARS`, which is only correct while that boundary is shared:
    /// were upstream to start counting bytes or display columns, a
    /// hundred-character branch would still shorten to something valid and
    /// this test is what would notice.
    #[test]
    fn fit_agrees_with_smit_about_where_the_limit_is() {
        let at = "x".repeat(Smit::MAX_CHARS);
        let under = "x".repeat(Smit::MAX_CHARS - 1);
        let over = "x".repeat(Smit::MAX_CHARS + 1);

        assert!(under.parse::<Smit>().is_ok(), "one under must be accepted");
        assert!(at.parse::<Smit>().is_ok(), "exactly at must be accepted");
        assert!(over.parse::<Smit>().is_err(), "one over must be refused");

        assert_eq!(fit(under.clone()), under, "under the limit is untouched");
        assert_eq!(fit(at.clone()), at, "at the limit is untouched");
        assert_eq!(
            fit(over).chars().count(),
            Smit::MAX_CHARS,
            "one over is shortened to exactly the limit"
        );
    }

    /// A [`Daemon`] whose only working method is `set_smit`, recording
    /// what it was asked to paint.
    struct Recording(std::cell::RefCell<Vec<(String, String)>>);

    impl Daemon for Recording {
        async fn set_smit(&self, sheep: &str, text: &str) -> Result<(), Error> {
            self.0
                .borrow_mut()
                .push((sheep.to_owned(), text.to_owned()));
            Ok(())
        }

        crate::fixtures::daemon_methods!(unimplemented;
            dog_config, list_flock, describe, start, delete, reload, restart, save_roll, sheep_config,
        );
    }

    /// fails if `publish` asks the daemon for the wrong sheep, or for text
    /// other than what `text` renders.
    #[tokio::test]
    async fn publish_sends_the_named_sheeps_own_text() {
        let daemon = Recording(std::cell::RefCell::new(Vec::new()));
        let state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        publish(&daemon, "web", &state).await.expect("publishes");
        assert_eq!(
            daemon.0.into_inner(),
            vec![("web".to_owned(), "▲ main@a1b2c3".to_owned())]
        );
    }

    /// fails if a repaint sends a request when the text has not moved. The
    /// loop calls it for every target after every deploy attempt, and most
    /// of those change nothing.
    #[tokio::test]
    async fn a_repaint_with_nothing_new_sends_nothing() {
        let daemon = Recording(std::cell::RefCell::new(Vec::new()));
        let state = target(Watch::Auto, Some("a1b2c3d4e5f6"));
        repaint(&daemon, "web", &text(&state), &state)
            .await
            .expect("repaints");
        assert_eq!(daemon.0.into_inner(), vec![]);
    }

    /// fails if a sha that moved after the first paint is not painted, which
    /// leaves `shep flock` on the old sha until the next tick.
    #[tokio::test]
    async fn a_moved_sha_is_repainted_with_the_new_text() {
        let daemon = Recording(std::cell::RefCell::new(Vec::new()));
        let before = text(&target(Watch::Auto, Some("a1b2c3d4e5f6")));
        let now = target(Watch::Auto, Some("f6e5d4c3b2a1"));
        repaint(&daemon, "web", &before, &now)
            .await
            .expect("repaints");
        assert_eq!(
            daemon.0.into_inner(),
            vec![("web".to_owned(), "▲ main@f6e5d4".to_owned())]
        );
    }
}
