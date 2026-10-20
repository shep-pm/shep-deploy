//! Changing the branch a target tracks, without deploying.

use crate::error::Error;
use crate::lock;
use crate::paths::Tree;
use crate::state::State;

/// Points the target at `branch` and lifts the hold on its last failed sha,
/// without deploying.
///
/// Takes no [`crate::daemon::Daemon`] and does not contact the remote, so a
/// name the remote does not have is reported by the next tick rather than
/// here. `failed` names a sha of the branch being left, so it is cleared with
/// the move.
///
/// Refused while the record names no released sha. `optin::cut_over` writes
/// its in-memory record whole and does not adopt `branch`, so a branch set
/// before the cutover would be overwritten by it.
///
/// Writes under the record lock and not the tree lock, for the reason
/// [`crate::deploy::set_watch`] gives; a deploy in flight adopts `branch` from
/// disk before its own write, so the move survives it.
///
/// # Errors
/// [`Error::Config`] if the record names no released sha, or if `branch` is
/// empty, begins with `-` or carries a control character - see
/// [`State::validate`]. [`Error::Io`] or [`Error::Config`] if `deploy.toml`
/// cannot be read or written.
///
/// Answers the branch the record carried before, and the record as it is after.
pub fn set_branch(tree: &Tree, branch: &str) -> Result<(String, State), Error> {
    let _record = lock::hold_record(tree)?;
    let mut state = State::read(&tree.state_file())?;

    if state.deployed.is_none() {
        return Err(Error::Config(format!(
            "{sheep} has a deploy tree but was never cut over to it, so a branch set now \
             would be overwritten by the cutover. Finish the cutover instead - remove {root} \
             and run `shep-deploy setup {sheep}`.",
            sheep = tree.sheep(),
            root = tree.root().display(),
        )));
    }

    let was = std::mem::replace(&mut state.branch, branch.to_owned());
    state.failed = None;
    // `State::write` validates, and refuses before it touches the file.
    state.write(&tree.state_file())?;
    Ok((was, state))
}

/// Sets `tree`'s branch and says so, for the `--branch` verb.
///
/// # Errors
/// As [`set_branch`].
pub fn branch_once(tree: &Tree, branch: &str) -> Result<u8, Error> {
    let (was, state) = set_branch(tree, branch)?;
    println!(
        "{} branch: {was} -> {}, still deployed at {}",
        tree.sheep(),
        state.branch,
        state.deployed.as_deref().unwrap_or("nothing yet")
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::paths::Tree;
    use crate::state::{State, Verify, Watch};
    use crate::{fixtures, lock};

    use super::*;

    /// A tree whose record names a release, a held sha, and settings that are
    /// not the defaults, so a rewrite that drops one shows.
    fn deployed_tree(home: &std::path::Path) -> Tree {
        let tree = Tree::for_sheep(home, "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        State {
            deployed: Some(fixtures::SHA.to_owned()),
            failed: Some(fixtures::OTHER_SHA.to_owned()),
            watch: Watch::Auto,
            verify: Verify::Alive,
            ..fixtures::state()
        }
        .write(&tree.state_file())
        .expect("the record before");
        tree
    }

    /// fails if the new branch is not what the next tick reads, or the hold on
    /// the old branch's failed sha outlives the move. `failed` names a sha of
    /// the branch being left, and the loop would compare it against the new
    /// branch's head for no reason.
    #[test]
    fn a_branch_is_rewritten_and_the_hold_is_lifted() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = deployed_tree(home.path());

        let (was, after) = set_branch(&tree, "release").expect("sets");

        assert_eq!(was, "main", "what the record carried before");
        let written = State::read(&tree.state_file()).expect("reads");
        assert_eq!(written, after, "the record returned is the one written");
        assert_eq!(written.branch, "release");
        assert_eq!(written.failed, None);
        assert_eq!(written.deployed.as_deref(), Some(fixtures::SHA));
        assert_eq!(written.watch, Watch::Auto);
        assert_eq!(written.verify, Verify::Alive);
    }

    /// fails if a name git would read as an option, or no name at all, is
    /// written to a record every later command then refuses to read.
    #[test]
    fn a_branch_that_begins_with_a_dash_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = deployed_tree(home.path());

        let err = set_branch(&tree, "--upload-pack=x").expect_err("refuses");

        assert!(matches!(err, Error::Config(_)), "{err}");
        let on_disk = State::read(&tree.state_file()).expect("still reads");
        assert_eq!(on_disk.branch, "main", "unchanged on disk");
        assert_eq!(on_disk.failed.as_deref(), Some(fixtures::OTHER_SHA));
    }

    /// fails if an empty or blank name is accepted. It would reach git as
    /// `refs/heads/` and the first tick would report a branch with no name.
    #[test]
    fn an_empty_branch_is_refused() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = deployed_tree(home.path());

        for name in ["", "  "] {
            let err = set_branch(&tree, name).expect_err("refuses");
            assert!(matches!(err, Error::Config(_)), "{name:?}: {err}");
        }
        let on_disk = State::read(&tree.state_file()).expect("still reads");
        assert_eq!(on_disk.branch, "main", "unchanged on disk");
    }

    /// fails if a tree the cutover never landed on can be retargeted.
    /// `optin::cut_over` writes its in-memory record whole and does not adopt
    /// `branch`, so a branch set before the cutover would be overwritten, and
    /// the operator would think it had held.
    #[test]
    fn a_tree_the_cutover_never_landed_on_cannot_be_retargeted() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        State {
            watch: Watch::Manual,
            ..fixtures::state()
        }
        .write(&tree.state_file())
        .expect("the record before");

        let err = set_branch(&tree, "release").expect_err("refuses");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("shep-deploy setup web"), "{shown}");
        assert_eq!(
            State::read(&tree.state_file()).expect("reads").branch,
            "main",
            "unchanged on disk"
        );
    }

    /// fails if retargeting skips the record lock a deploy's write holds.
    #[test]
    fn a_branch_waits_for_the_record_lock() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = deployed_tree(home.path());
        let held = lock::hold_record(&tree).expect("the deploy's hold");
        let (tx, rx) = std::sync::mpsc::channel();
        let operator = {
            let home = home.path().to_owned();
            std::thread::spawn(move || {
                let result = set_branch(&Tree::for_sheep(&home, "web"), "release");
                tx.send(()).expect("report");
                result
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "set_branch must wait for the record lock"
        );

        drop(held);

        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("set_branch lands");
        operator.join().expect("thread").expect("sets");
    }
}
