//! Whether a push touches anything a target runs from.
//!
//! One repository can be several targets, and the poll loop compares only
//! shas, so a push to any of them reloaded all of them. A target that names
//! the paths it runs from in `[dog.deploy.reload]` is left alone by a push
//! that changes none of them. See [`changed`].

use std::path::Path;

use serde::Deserialize;

use crate::error::Error;
use crate::paths::Tree;
use crate::shared::{self, FromCheckout, printable};
use crate::state::State;
use crate::{flockfile, git, hook};

/// The file that is always one of a target's paths: a push that edits the
/// table has to be able to deploy, or a bad table could never be repaired.
const FLOCKFILE: &str = "Flockfile.toml";

/// The `[dog.deploy.reload]` block: what a target runs from.
///
/// Unknown keys are refused rather than ignored, as in
/// [`crate::build::BuildSpec`]: a typo would otherwise be a filter that is
/// never applied, and every push would go on deploying with nothing saying so.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReloadSpec {
    /// Git pathspecs, relative to the repository root. A directory name
    /// matches its contents. Empty means every new sha deploys.
    pub paths: Vec<String>,
}

/// The `[dog.deploy.reload]` block of `doc`, or the default if it has none.
///
/// # Errors
/// [`Error::Config`] naming `release` if the block does not match
/// [`ReloadSpec`]'s schema: an unknown key, or a value of the wrong type.
fn parse(doc: &toml::Value, release: &Path) -> Result<ReloadSpec, Error> {
    let Some(table) = hook::deploy_table(doc, "reload") else {
        return Ok(ReloadSpec::default());
    };
    table.clone().try_into().map_err(|source: toml::de::Error| {
        Error::Config(format!(
            "{}: `[dog.deploy.reload]` does not match the reload schema: {}",
            release.display(),
            printable(source.message())
        ))
    })
}

/// Whether `to` changes anything the live `release` declares it runs from,
/// compared with `from`.
///
/// `true` is "deploy it": it is the answer whenever the release declares no
/// paths, so a target that has not opted in behaves as it always did.
/// `Flockfile.toml` counts as one of the paths whatever the table says.
///
/// # Errors
/// Whatever [`flockfile::read`] returns for `release`, [`Error::Config`] if
/// its `[dog.deploy.reload]` is malformed, and [`Error::Git`] if `from` or
/// `to` is not in `git_dir`. The caller deploys on any of them.
pub(crate) fn changed(
    git_dir: &Path,
    release: &Path,
    shared: &FromCheckout,
    from: &str,
    to: &str,
) -> Result<bool, Error> {
    let flockfiles = flockfile::read(release, shared)?;
    let mut paths = parse(&flockfiles.doc, release)?.paths;
    if paths.is_empty() {
        return Ok(true);
    }
    paths.push(FLOCKFILE.to_owned());
    git::changed(git_dir, from, to, &paths)
}

/// Whether `head` can be left undeployed because nothing under the live
/// release's reload paths differs from what is deployed.
///
/// Any failure to decide answers `false` and says why, so the push deploys as
/// it would have without this check.
pub(crate) async fn nothing_changed(tree: &Tree, state: &State, live: &Path, head: &str) -> bool {
    let Some(deployed) = state.deployed.clone() else {
        return false;
    };
    let (git_dir, live, checkout, head_sha) = (
        tree.git(),
        live.to_owned(),
        state.checkout.clone(),
        head.to_owned(),
    );
    let decided = shared::off_thread(move || {
        // The same evidence a staged release is read with: whether the
        // override in `live` is the operator's is a fact about their checkout.
        let linked = shared::to_link(&checkout)?;
        changed(&git_dir, &live, &linked, &deployed, &head_sha)
    })
    .await;
    match decided {
        Ok(changed) => !changed,
        Err(err) => {
            eprintln!(
                "shep-deploy: {}: could not tell whether {head} touches the reload paths, \
                 deploying it: {err}",
                tree.sheep()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::fixtures;
    use crate::shared::FromCheckout;

    use super::*;

    const APP: &str = "[[app]]\nname = \"web\"\nscript = \"./run\"\n";

    /// A release whose Flockfile is the app plus `extra`.
    fn release_with(extra: &str) -> tempfile::TempDir {
        fixtures::fixture_release(&[("Flockfile.toml", &format!("{APP}{extra}"))])
    }

    const VOTING: &str = "[dog.deploy.reload]\npaths = [\"cmd/voting\", \"assets/*.png\"]\n";

    /// A repository with two commits, the second touching `files`; returns it
    /// with the shas of both.
    fn pushed(files: &[(&str, &str)]) -> (tempfile::TempDir, String, String) {
        let repo = fixtures::checkout(&[
            ("cmd/voting/main.go", "1"),
            ("cmd/bot/main.go", "1"),
            ("assets/logo.png", "1"),
        ]);
        let from = fixtures::head_of(repo.path());
        fixtures::commit(repo.path(), files, "push");
        let to = fixtures::head_of(repo.path());
        (repo, from, to)
    }

    fn decide(release: &tempfile::TempDir, repo: &tempfile::TempDir, from: &str, to: &str) -> bool {
        changed(
            repo.path(),
            release.path(),
            &FromCheckout::of(Vec::new()),
            from,
            to,
        )
        .expect("decides")
    }

    /// fails if a push that touches nothing a target declared still deploys,
    /// which is the whole of what this feature is for.
    #[test]
    fn a_push_outside_the_paths_is_unchanged() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        assert!(!decide(&release_with(VOTING), &repo, &from, &to));
    }

    /// fails if a push into a declared directory, or onto a declared glob,
    /// stops deploying.
    #[test]
    fn a_push_inside_a_path_is_changed() {
        let release = release_with(VOTING);

        let (repo, from, to) = pushed(&[("cmd/voting/main.go", "2")]);
        assert!(
            decide(&release, &repo, &from, &to),
            "a directory name matches its contents"
        );

        let (repo, from, to) = pushed(&[("assets/logo.png", "2")]);
        assert!(decide(&release, &repo, &from, &to), "a glob");
    }

    /// fails if a push that edits the table itself can be skipped by the
    /// table it is replacing.
    #[test]
    fn an_edit_to_the_flockfile_is_always_changed() {
        let (repo, from, to) = pushed(&[("Flockfile.toml", "edited")]);
        assert!(decide(&release_with(VOTING), &repo, &from, &to));
    }

    /// fails if a Flockfile that declares nothing starts skipping pushes.
    /// Both spellings of nothing mean today's behaviour.
    #[test]
    fn no_table_and_an_empty_list_both_mean_deploy() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        assert!(decide(&release_with(""), &repo, &from, &to), "no table");
        assert!(
            decide(
                &release_with("[dog.deploy.reload]\npaths = []\n"),
                &repo,
                &from,
                &to
            ),
            "an empty list"
        );
        assert!(
            decide(&release_with("[dog.deploy.reload]\n"), &repo, &from, &to),
            "an empty table"
        );
    }

    /// fails if a typo in the table is ignored: the target would then deploy
    /// on every push and nothing would say the filter was never read.
    #[test]
    fn an_unknown_key_in_the_table_is_refused_by_name() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        let release = release_with("[dog.deploy.reload]\npths = [\"cmd/voting\"]\n");

        let err = changed(
            repo.path(),
            release.path(),
            &FromCheckout::of(Vec::new()),
            &from,
            &to,
        )
        .expect_err("a typo is refused");

        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(err.to_string().contains("pths"), "{err}");
    }
}
