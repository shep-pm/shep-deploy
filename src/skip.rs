//! Whether a push has anything a sheep runs from in it.
//!
//! One repository can be several sheep, and the poll loop compares only
//! shas, so a push to any of them reloaded all of them. Two answers say it
//! did not: nothing under the sheep's `reload` pathspecs changed, and the
//! build produced the `script` the sheep is already serving. The first
//! needs a path list and works for any app; the second needs none and is
//! exact only for a compiled one. See [`changed`] and [`serving_script`].

use std::fs;
use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::error::Error;
use crate::git;
use crate::paths::Tree;
use crate::shared;
use crate::state::State;

/// Why a deploy was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// Nothing under the sheep's `reload` pathspecs changed. See [`nothing_changed`].
    ReloadPaths,
    /// The build produced the `script` the sheep is already serving. See
    /// [`serving_script`] and [`built_script_matches`].
    UnchangedScript,
}

impl Reason {
    /// The tail of the line that says a skip, after `<sheep> skipped <sha>,`.
    ///
    /// One place spells it so the poll loop and a deploy asked for by name
    /// cannot drift apart on what a skip was for.
    pub(crate) const fn phrase(&self) -> &'static str {
        match self {
            Self::ReloadPaths => "nothing under its reload paths changed",
            Self::UnchangedScript => "its built script is unchanged",
        }
    }
}

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

/// `app`'s script inside `release`, or `None` when it names no file inside one.
///
/// The cutover registers the sheep at the tree's `current` symlink, so a
/// relative `script` is the same path inside every release of that tree. An
/// absolute one, or a `~` one, names a file no build of this repository wrote,
/// and comparing it would compare one file with itself.
fn script_in(release: &Path, app: &AppConfig) -> Option<PathBuf> {
    let script = Path::new(&app.script);
    (!script.is_absolute() && !app.script.starts_with('~')).then(|| release.join(script))
}

/// `release`'s script bytes, or why it has none to compare.
fn read_script(release: &Path, app: &AppConfig) -> Result<Vec<u8>, Error> {
    let Some(path) = script_in(release, app) else {
        return Err(Error::Config(format!(
            "`{}` is not a path inside a release, so there is no built script to \
             compare; `script` is compared as the file each release holds at that path",
            shared::printable(&app.script)
        )));
    };
    fs::read(&path).map_err(Error::at(&path))
}

/// The script bytes `live` is serving.
///
/// Read before the build rather than at the compare, which is the whole of why
/// it is its own step: a release's `target` is a symlink at the repository's
/// shared build cache, so a script under it is one file every release of that
/// remote resolves to, and the build has already replaced it by the time the
/// compare runs. Reading it then would compare a build with itself and skip
/// every commit.
///
/// `None` is "nothing to compare against", said out loud. The caller deploys.
pub(crate) async fn serving_script(sheep: &str, app: &AppConfig, live: &Path) -> Option<Vec<u8>> {
    let (app, live) = (app.clone(), live.to_owned());
    match shared::off_thread(move || read_script(&live, &app)).await {
        Ok(bytes) => Some(bytes),
        Err(why) => {
            eprintln!("shep-deploy: {sheep}: cannot compare its built script, deploying it: {why}");
            None
        }
    }
}

/// Whether `release`'s build produced `serving` byte for byte.
///
/// A failure to read `release`'s script answers `false` and says why, so a
/// script the dog cannot read deploys as it always did.
pub(crate) async fn built_script_matches(
    sheep: &str,
    app: &AppConfig,
    release: &Path,
    serving: &[u8],
) -> bool {
    let (app, release, serving) = (app.clone(), release.to_owned(), serving.to_vec());
    match shared::off_thread(move || read_script(&release, &app)).await {
        Ok(bytes) => bytes == serving,
        Err(why) => {
            eprintln!("shep-deploy: {sheep}: cannot compare its built script, deploying it: {why}");
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
