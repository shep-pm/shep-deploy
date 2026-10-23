//! Whether a push touches anything a sheep runs from.
//!
//! One repository can be several sheep, and the poll loop compares only
//! shas, so a push to any of them reloaded all of them. A sheep whose
//! `reload` setting names the paths it runs from is left alone by a push
//! that changes none of them. See [`changed`].

use std::path::Path;

use crate::error::Error;
use crate::git;
use crate::paths::Tree;
use crate::shared;
use crate::state::State;

/// Whether `to` changes anything under the `reload` pathspecs, compared with
/// `from`.
///
/// `true` is "deploy it": it is the answer whenever `reload` is empty, so a
/// sheep that has not opted in behaves as it always did.
///
/// # Errors
/// [`Error::Git`] if `from` or `to` is not in `git_dir`, or a pathspec is not
/// one git accepts. The caller deploys on either.
pub(crate) fn changed(
    git_dir: &Path,
    reload: &[String],
    from: &str,
    to: &str,
) -> Result<bool, Error> {
    if reload.is_empty() {
        return Ok(true);
    }
    git::changed(git_dir, from, to, reload)
}

/// Whether `head` can be left undeployed because nothing under the sheep's
/// `reload` paths differs from what is deployed.
///
/// Any failure to decide answers `false` and says why, so the push deploys as
/// it would have without this check.
pub(crate) async fn nothing_changed(
    tree: &Tree,
    state: &State,
    reload: &[String],
    head: &str,
) -> bool {
    let Some(deployed) = state.deployed.clone() else {
        return false;
    };
    let (git_dir, reload, head_sha) = (tree.git(), reload.to_vec(), head.to_owned());
    let decided =
        shared::off_thread(move || changed(&git_dir, &reload, &deployed, &head_sha)).await;
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
    use crate::fixtures;

    use super::*;

    fn voting() -> Vec<String> {
        vec!["cmd/voting".to_owned(), "assets/*.png".to_owned()]
    }

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

    fn decide(reload: &[String], repo: &tempfile::TempDir, from: &str, to: &str) -> bool {
        changed(repo.path(), reload, from, to).expect("decides")
    }

    /// fails if a push that touches nothing a sheep declared still deploys,
    /// which is the whole of what this feature is for.
    #[test]
    fn a_push_outside_the_paths_is_unchanged() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        assert!(!decide(&voting(), &repo, &from, &to));
    }

    /// fails if a push into a declared directory, or onto a declared glob,
    /// stops deploying.
    #[test]
    fn a_push_inside_a_path_is_changed() {
        let (repo, from, to) = pushed(&[("cmd/voting/main.go", "2")]);
        assert!(
            decide(&voting(), &repo, &from, &to),
            "a directory name matches its contents"
        );

        let (repo, from, to) = pushed(&[("assets/logo.png", "2")]);
        assert!(decide(&voting(), &repo, &from, &to), "a glob");
    }

    /// fails if the Flockfile is still treated as one of the paths: it
    /// configures nothing the sheep runs from any more, so a push that edits
    /// only it is outside them.
    #[test]
    fn an_edit_to_the_flockfile_is_not_a_declared_path() {
        let (repo, from, to) = pushed(&[("Flockfile.toml", "edited")]);
        assert!(!decide(&voting(), &repo, &from, &to));
    }

    /// fails if a sheep that declares nothing starts skipping pushes.
    #[test]
    fn an_empty_list_means_deploy() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        assert!(decide(&[], &repo, &from, &to));
    }

    /// fails if a pathspec git rejects is read as "nothing changed", which
    /// would skip every push for a typo.
    #[test]
    fn a_pathspec_git_rejects_is_an_error() {
        let (repo, from, to) = pushed(&[("cmd/bot/main.go", "2")]);
        let err = changed(repo.path(), &[":(bogus)cmd".to_owned()], &from, &to)
            .expect_err("git refuses it");
        assert!(matches!(err, Error::Git { .. }), "{err}");
    }
}
