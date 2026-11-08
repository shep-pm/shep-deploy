//! Whether a push has anything a sheep runs from in it.
//!
//! One repository can be several sheep, and the poll loop compares only
//! shas, so a push to any of them reloaded all of them. Two answers say it
//! did not: nothing under the sheep's `reload` pathspecs changed, and the
//! build produced the same `script` this sheep last landed. The first needs a
//! path list and works for any app; the second needs none and is exact only
//! for a compiled one. See [`changed`] and [`stamp`].
//!
//! # The second answer is a comparison with a record, not with the live release
//!
//! A release's `target` is a symlink at its repository's shared build cache
//! (see [`crate::paths`]), so a `script` under it resolves to one file every
//! release of that remote shares, and whichever sheep built last owns its
//! bytes. Read that path and you have the cache, not what this sheep's process
//! execs. The poll loop deploys sheep one after another, so by the time the
//! second asks, the first has already overwritten it, and a comparison against
//! the live release would skip the reload for a sheep still running the old
//! binary. What this tree's own last landing recorded in
//! [`State::served_script`] is the half that identifies this sheep.
//!
//! Both sides of the compare are therefore digests rather than files, each one
//! taken inside this tree's own build turn: the recorded one by the deploy that
//! landed it, the asking one by the deploy that is asking. Nothing reads the
//! path once the turn has been let go.

use std::fs;
use std::path::{Path, PathBuf};

use shep_client::shep_core::config::AppConfig;

use crate::error::Error;
use crate::git;
use crate::paths::{self, Tree};
use crate::shared;
use crate::state::State;

/// Why a deploy was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// Nothing under the sheep's `reload` pathspecs changed. See [`nothing_changed`].
    ReloadPaths,
    /// The build produced the `script` this sheep last landed. See [`stamp`]
    /// and [`State::served_script`].
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

/// `release`'s script digest, or why it has none to take.
fn stamped(release: &Path, app: &AppConfig) -> Result<String, Error> {
    let Some(path) = script_in(release, app) else {
        return Err(Error::Config(format!(
            "`{}` is not a path inside a release, so there is no built script to \
             compare; `script` is compared as the file each release holds at that path",
            shared::printable(&app.script)
        )));
    };
    Ok(paths::digest(&fs::read(&path).map_err(Error::at(&path))?))
}

/// What `release`'s `script` looks like, as [`State::served_script`] stores it.
///
/// One caller, and it is the one moment the file is that deploy's to describe:
/// `crate::deploy`'s own attempt takes it after its build, inside the
/// repository's build turn, so no other sheep's compiler is inside the cache,
/// and hands the answer to its compare and to the record its landing writes. At
/// any other moment the digest belongs to whichever sheep built last, which is
/// why nothing downstream re-reads it.
///
/// Off the runtime's thread: it reads the whole file, which for a compiled app
/// is a binary of some megabytes.
///
/// # Errors
/// [`Error::Config`] when `script` names no file inside a release, and
/// [`Error::Io`] naming that path when it cannot be read. Its caller prints why
/// and deploys when the sheep asked for the compare, and records nothing when
/// it did not, which the next compare reads as "nothing to compare with".
pub(crate) async fn stamp(app: &AppConfig, release: &Path) -> Result<String, Error> {
    let (app, release) = (app.clone(), release.to_owned());
    shared::off_thread(move || stamped(&release, &app)).await
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

    /// An app whose `script` is `./app`, for the two questions below: which
    /// file a release's digest is taken from, and what happens when `script`
    /// names no file inside a release at all.
    fn app_with(script: &str) -> AppConfig {
        let mut app = fixtures::gated_app("web", "");
        app.script = script.to_owned();
        app
    }

    /// fails if the digest is taken from anywhere but the file `script` names
    /// inside the release, which is the file the sheep is launched from.
    #[test]
    fn a_releases_digest_is_of_the_file_its_script_names() {
        let release = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(release.path().join("target/release")).expect("dir");
        std::fs::write(release.path().join("target/release/app"), "one").expect("write");

        let stamp = stamped(release.path(), &app_with("./target/release/app")).expect("digest");
        assert_eq!(stamp, paths::digest(b"one"));
    }

    /// fails if an absolute `script`, or a `~` one, is compared at all: it names
    /// a file no build of this repository wrote, so the compare would be one
    /// file against itself and every commit would look unchanged.
    #[test]
    fn a_script_outside_a_release_has_nothing_to_compare() {
        let release = tempfile::tempdir().expect("tempdir");
        for script in ["/usr/local/bin/app", "~/bin/app"] {
            let err = stamped(release.path(), &app_with(script))
                .expect_err(&format!("must refuse {script}"));
            assert!(matches!(err, Error::Config(_)), "{script}: {err}");
            assert!(
                err.to_string().contains("not a path inside a release"),
                "{err}"
            );
        }
    }
}
