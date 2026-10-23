//! Changing the branch a target tracks, without deploying.

use crate::daemon::Daemon;
use crate::error::Error;
use crate::lock;
use crate::paths::Tree;
use crate::settings::{self, Change, Settings};
use crate::state::{State, branch_refusal};

/// Points the target at `branch`, a branch name or a full ref such as
/// `refs/pull/1/head`, and lifts the hold on its last failed sha,
/// without deploying.
///
/// The branch is written to the sheep's `[app.dogs.deploy]` table, and the
/// only request made of the shepherd besides reading that table is the write,
/// so nothing is started, reloaded or restarted. It does not contact the
/// remote, so a name the remote does not have is reported by the next tick
/// rather than here. `failed` names a sha of the branch being left, so it is
/// cleared with the move.
///
/// Refused while the record names no released sha: the first release was built
/// from the branch setup read, so a branch set before the cutover lands would
/// not describe the release it lands.
///
/// The table is written first and the record second, so a failure between
/// them leaves the move made and a hold on a sha of the old branch, which
/// the next tick compares against the new branch's head to no effect. The
/// other order would lift the hold with the branch unmoved. The record is
/// written under the record lock and not the tree lock, for the reason
/// [`crate::deploy::set_watch`] gives.
///
/// # Errors
/// [`Error::Config`] if the record names no released sha, or if `branch` is
/// empty, begins with `-`, carries a control character or is a `refs/` value
/// git would refuse - see [`branch_refusal`]. [`Error::Io`] or
/// [`Error::Config`] if `deploy.toml` cannot be read or written, and
/// whatever [`settings::read`], [`settings::change`] and [`State::adopt`]
/// return.
///
/// Answers the branch the table held before, if it held one, and the record
/// as it is after.
pub async fn set_branch<D: Daemon>(
    daemon: &D,
    tree: &Tree,
    branch: &str,
) -> Result<(Option<String>, State), Error> {
    let sheep = tree.sheep();
    if let Some(why) = branch_refusal(branch) {
        return Err(Error::Config(format!("{sheep}: `branch` {why}")));
    }

    let held = settings::read(daemon, sheep).await?;
    if State::read(&tree.state_file())?.deployed.is_none() {
        return Err(Error::Config(format!(
            "{sheep} has a deploy tree but was never cut over to it, so a branch set now \
             would not describe the release the cutover lands. Finish the cutover instead - \
             remove {root} and run `shep-deploy setup {sheep}`.",
            root = tree.root().display(),
        )));
    }

    settings::change(daemon, sheep, [Change::Branch(branch.to_owned())]).await?;

    let _record = lock::hold_record(tree)?;
    let mut state = State::read(&tree.state_file())?;
    state.adopt(
        sheep,
        &Settings {
            branch: Some(branch.to_owned()),
            ..held.clone()
        },
    )?;
    state.failed = None;
    state.write(&tree.state_file())?;
    Ok((held.branch, state))
}

/// Sets `tree`'s branch and says so, for the `--branch` verb.
///
/// # Errors
/// As [`set_branch`].
pub async fn branch_once<D: Daemon>(daemon: &D, tree: &Tree, branch: &str) -> Result<u8, Error> {
    let (was, state) = set_branch(daemon, tree, branch).await?;
    println!(
        "{} branch: {} -> {}, still deployed at {}",
        tree.sheep(),
        was.as_deref().unwrap_or("none"),
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

    /// The sheep's table: a branch, settings that are not the defaults, and a
    /// build, so a rewrite that drops one shows.
    const TABLE: &str = "branch = 'main'\nwatch = 'auto'\nverify = 'alive'\ncommand = 'make'\n";

    /// A tree whose record names a release and a held sha, and the sheep
    /// whose table it is read against.
    fn deployed_tree(home: &std::path::Path) -> (Tree, fixtures::Sheep) {
        let tree = Tree::for_sheep(home, "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        State {
            deployed: Some(fixtures::SHA.to_owned()),
            failed: Some(fixtures::OTHER_SHA.to_owned()),
            ..fixtures::state()
        }
        .write(&tree.state_file())
        .expect("the record before");
        (tree, fixtures::Sheep::of("web", TABLE))
    }

    /// fails if the new branch is not what the next tick reads, if the hold
    /// on the old branch's failed sha outlives the move, or if the write
    /// drops a key of the table it did not mean to touch. `failed` names a
    /// sha of the branch being left, and the loop would compare it against
    /// the new branch's head for no reason.
    #[tokio::test]
    async fn a_branch_is_written_to_the_table_and_the_hold_is_lifted() {
        let home = tempfile::tempdir().expect("tempdir");
        let (tree, sheep) = deployed_tree(home.path());

        let (was, after) = set_branch(&sheep, &tree, "release").await.expect("sets");

        assert_eq!(was.as_deref(), Some("main"), "what the table held before");
        let held = sheep.settings();
        assert_eq!(held.branch.as_deref(), Some("release"));
        assert_eq!(held.watch, Watch::Auto);
        assert_eq!(held.verify, Verify::Alive);
        assert_eq!(held.command.as_deref(), Some("make"));
        let written = State::read(&tree.state_file()).expect("reads");
        assert_eq!(written.failed, None);
        assert_eq!(written.deployed.as_deref(), Some(fixtures::SHA));
        assert_eq!(after.branch, "release", "the record returned is adopted");
        assert_eq!(after.failed, None);
        let on_disk = std::fs::read_to_string(tree.state_file()).expect("reads");
        assert!(!on_disk.contains("release"), "{on_disk}");
    }

    /// fails if a full ref, which is how a pull request head is followed, is
    /// refused or rewritten on its way into the table.
    #[tokio::test]
    async fn a_full_ref_is_written_as_given() {
        let home = tempfile::tempdir().expect("tempdir");
        let (tree, sheep) = deployed_tree(home.path());

        set_branch(&sheep, &tree, "refs/pull/1/head")
            .await
            .expect("sets");

        assert_eq!(sheep.settings().branch.as_deref(), Some("refs/pull/1/head"));
    }

    /// fails if a name git would read as an option, or no name at all, is
    /// written to a table every later tick then refuses to adopt.
    #[tokio::test]
    async fn a_branch_that_cannot_be_followed_is_refused_and_nothing_is_written() {
        let home = tempfile::tempdir().expect("tempdir");
        let (tree, sheep) = deployed_tree(home.path());

        for name in ["--upload-pack=x", "", "  ", "refs/heads/a..b"] {
            let err = set_branch(&sheep, &tree, name).await.expect_err("refuses");
            assert!(matches!(err, Error::Config(_)), "{name:?}: {err}");
        }

        assert_eq!(sheep.settings().branch.as_deref(), Some("main"));
        let on_disk = State::read(&tree.state_file()).expect("still reads");
        assert_eq!(on_disk.failed.as_deref(), Some(fixtures::OTHER_SHA));
    }

    /// fails if a tree the cutover never landed on can be retargeted: the
    /// first release was built from the branch setup read.
    #[tokio::test]
    async fn a_tree_the_cutover_never_landed_on_cannot_be_retargeted() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        fixtures::state()
            .write(&tree.state_file())
            .expect("the record before");
        let sheep = fixtures::Sheep::of("web", TABLE);

        let err = set_branch(&sheep, &tree, "release")
            .await
            .expect_err("refuses");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("never cut over"), "{shown}");
        assert!(shown.contains("shep-deploy setup web"), "{shown}");
        assert_eq!(sheep.settings().branch.as_deref(), Some("main"));
    }

    /// fails if retargeting skips the record lock a deploy's write holds.
    #[test]
    fn a_branch_waits_for_the_record_lock() {
        let home = tempfile::tempdir().expect("tempdir");
        let (tree, _) = deployed_tree(home.path());
        let held = lock::hold_record(&tree).expect("the deploy's hold");
        let (tx, rx) = std::sync::mpsc::channel();
        let operator = {
            let home = home.path().to_owned();
            std::thread::spawn(move || {
                let sheep = fixtures::Sheep::of("web", TABLE);
                let result = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("runtime")
                    .block_on(set_branch(
                        &sheep,
                        &Tree::for_sheep(&home, "web"),
                        "release",
                    ));
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
