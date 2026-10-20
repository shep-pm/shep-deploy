//! Linked paths that look like build output.
//!
//! A deploy links every gitignored path the operator's `.shepignore` does not
//! name, so a build directory or a declared artifact in the checkout is shared
//! into every release. This module only names them. `target` is not in the
//! list: [`crate::shared::link_cache`] claims it first.
//!
//! [`warnings`] judges a path by its name, before the build. [`Snapshot`]
//! judges it by what the build did, for the build that writes into a linked
//! path under a name nothing here knows, a Go binary at the repository root
//! for one.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::shared::{FromCheckout, printable};

const BUILD_DIRECTORIES: [&str; 4] = ["node_modules", ".output", "dist", ".next"];

/// One line per linked path that looks like build output, each saying why and
/// how to stop linking it. Empty when nothing does.
///
/// A path is named when a whole component of it is a build directory name, or
/// when it and a `build.artifacts` entry are equal or one holds the other.
/// Names the first reason found, so a path is never named twice.
pub(crate) fn warnings(sheep: &str, shared: &FromCheckout, artifacts: &[PathBuf]) -> Vec<String> {
    shared
        .iter()
        .filter_map(|path| {
            let reason = reason(path, artifacts)?;
            Some(format!(
                "shep-deploy: {sheep}: warning: `{}` is linked from the checkout into every \
                 release, and {reason}. Add it to `.shepignore` in the checkout to stop linking it.",
                printable(path.display()),
            ))
        })
        .collect()
}

fn reason(path: &Path, artifacts: &[PathBuf]) -> Option<String> {
    let names = names(path);
    if let Some(name) = names
        .iter()
        .find_map(|name| BUILD_DIRECTORIES.iter().find(|build| *name == **build))
    {
        return Some(format!(
            "it has a `{name}` component, which holds build output"
        ));
    }
    artifacts.iter().find_map(|artifact| {
        let declared = self::names(artifact);
        (!declared.is_empty() && (names.starts_with(&declared) || declared.starts_with(&names)))
            .then(|| {
                format!(
                    "it overlaps `build.artifacts` entry `{}`, which the build writes",
                    printable(artifact.display())
                )
            })
    })
}

/// What a linked path looked like, read through its link.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    modified: SystemTime,
    /// A regular file's length. `None` for a directory, which is judged by
    /// its modified time alone.
    len: Option<u64>,
}

/// `release/path` as the checkout's copy looks through the link, or `None`
/// when that cannot be said: nothing is there, the link dangles, the entry
/// is no longer a link, or the platform keeps no modified time.
///
/// A path that stopped being a link is the build having replaced it with a
/// file of its own. The checkout's copy is untouched and the release is
/// right, so it reads as nothing to report.
fn read(release: &Path, path: &Path) -> Option<Record> {
    let link = release.join(path);
    if !link.symlink_metadata().ok()?.is_symlink() {
        return None;
    }
    let meta = fs::metadata(&link).ok()?;
    Some(Record {
        modified: meta.modified().ok()?,
        len: meta.is_file().then_some(meta.len()),
    })
}

/// The linked paths of a release as they stood before its build, so the
/// build's writes through the links can be named afterwards.
///
/// A link makes the checkout's copy and the release's the same file, so a
/// build that writes into a linked path writes into the operator's checkout
/// and into every other release. [`warnings`] cannot see that when the path
/// has no build-directory name and no `build.artifacts` entry. The copy
/// changing during the build can.
///
/// A file is judged by its modified time and length. A directory is judged by
/// its own modified time only, which changes when an entry is added or removed
/// directly inside it. A write nested deeper, or into an existing file in it,
/// is not seen.
///
/// The record cannot say who wrote. The running release shares the same
/// links, so a file the app itself writes while the build runs, a database
/// or a log, reads as changed as well. The warning says so, and leaves the
/// decision to the operator.
#[derive(Debug)]
pub(crate) struct Snapshot {
    sheep: String,
    release: PathBuf,
    records: Vec<(PathBuf, Record)>,
}

impl Snapshot {
    /// Records every path in `shared` that can be read through its link in
    /// `release`. One that cannot is skipped, not an error: this only warns.
    #[must_use]
    pub(crate) fn before(sheep: &str, release: &Path, shared: &FromCheckout) -> Self {
        let records = shared
            .iter()
            .filter_map(|path| Some((path.clone(), read(release, path)?)))
            .collect();
        Self {
            sheep: sheep.to_owned(),
            release: release.to_owned(),
            records,
        }
    }

    /// One line per recorded path whose record is not what it was, saying the
    /// path changed while the build ran, that the build may have written
    /// through the link into the checkout, and how to stop linking it. Empty
    /// when nothing changed.
    ///
    /// A path that cannot be read now, or that is no longer a link, is silent.
    #[must_use]
    pub(crate) fn after(self) -> Vec<String> {
        let Self {
            sheep,
            release,
            records,
        } = self;
        records
            .into_iter()
            .filter(|(path, before)| read(&release, path).is_some_and(|now| now != *before))
            .map(|(path, _)| {
                format!(
                    "shep-deploy: {sheep}: warning: `{}` changed while the build ran. If the \
                     build wrote through the link into the checkout, where it is linked from \
                     into every release, add it to `.shepignore` in the checkout to stop \
                     linking it. If the running app writes it, this is expected and the \
                     path should stay linked.",
                    printable(path.display()),
                )
            })
            .collect()
    }
}

/// The named components of `path`, so `./out` and `out` compare equal.
fn names(path: &Path) -> Vec<&OsStr> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    /// A tempdir checkout and an empty release beside it, with `files`
    /// written into the checkout and each linked into the release.
    fn linked_release(files: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (checkout, release) = (dir.path().join("checkout"), dir.path().join("release"));
        fs::create_dir_all(&checkout).expect("checkout");
        fs::create_dir_all(&release).expect("release");
        for name in files {
            fs::write(checkout.join(name), "built by the last release").expect("write");
            symlink(checkout.join(name), release.join(name)).expect("link");
            age(&checkout.join(name));
        }
        (dir, checkout, release)
    }

    /// Sets the modified time of a file or directory well in the past, so a
    /// write made a moment later cannot land on the same timestamp tick.
    fn age(path: &Path) {
        fs::File::open(path)
            .expect("open")
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .expect("set_modified");
    }

    fn linked(paths: &[&str]) -> FromCheckout {
        FromCheckout::of(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn a_linked_build_directory_is_named() {
        for name in ["node_modules", ".output", "dist", ".next"] {
            for path in [name.to_string(), format!("packages/app/{name}")] {
                let found = warnings("web", &linked(&[&path]), &[]);
                assert_eq!(found.len(), 1, "{path}: {found:?}");
                assert!(found[0].starts_with("shep-deploy: web:"), "{found:?}");
                assert!(found[0].contains(&path), "{found:?}");
                assert!(found[0].contains(".shepignore"), "{found:?}");
            }
        }
    }

    #[test]
    fn a_linked_path_that_overlaps_an_artifact_is_named() {
        let artifacts = [PathBuf::from("out/bin/koji")];
        for path in ["out/bin/koji", "out", "out/bin/koji/debug"] {
            let found = warnings("web", &linked(&[path]), &artifacts);
            assert_eq!(found.len(), 1, "{path}: {found:?}");
            assert!(found[0].starts_with("shep-deploy: web:"), "{found:?}");
            assert!(found[0].contains("out/bin/koji"), "{found:?}");
            assert!(found[0].contains(".shepignore"), "{found:?}");
        }
    }

    #[test]
    fn an_ordinary_shared_file_is_not_named() {
        let artifacts = [PathBuf::from("out/bin/koji")];
        let found = warnings(
            "web",
            &linked(&[".env", "config/local.json", "Flockfile.override.toml"]),
            &artifacts,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_name_that_only_contains_a_build_directory_name_is_not_named() {
        let found = warnings("web", &linked(&["distinct", "my_dist", "dist.log"]), &[]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_artifact_that_names_no_component_overlaps_nothing() {
        let artifacts = [PathBuf::from(".")];
        let found = warnings("web", &linked(&[".env"]), &artifacts);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_warning_cannot_forge_a_line() {
        let path = "dist/x\nshep-deploy: web: forged";
        let found = warnings("web", &linked(&[path]), &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].contains('\n'), "{found:?}");
    }

    /// fails if a build writing through a link into the checkout goes
    /// unnamed. The Go case: the binary has no build-directory name and no
    /// `build.artifacts` entry, so only its copy changing shows it.
    #[test]
    fn a_build_that_writes_through_a_linked_file_is_named() {
        let (_dir, checkout, release) = linked_release(&["koji"]);
        let files = linked(&["koji"]);
        let snapshot = Snapshot::before("web", &release, &files);

        fs::write(release.join("koji"), "rebuilt").expect("write through");

        let found = snapshot.after();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("shep-deploy: web:"), "{found:?}");
        assert!(found[0].contains("`koji`"), "{found:?}");
        assert!(
            found[0].contains("changed while the build ran"),
            "{found:?}"
        );
        assert!(found[0].contains("the running app"), "{found:?}");
        assert!(found[0].contains(".shepignore"), "{found:?}");
        assert_eq!(
            fs::read_to_string(checkout.join("koji")).expect("read"),
            "rebuilt"
        );
    }

    #[test]
    fn a_build_that_leaves_a_linked_file_alone_is_silent() {
        let (_dir, _checkout, release) = linked_release(&[".env", "config.json"]);
        let files = linked(&[".env", "config.json"]);
        let snapshot = Snapshot::before("web", &release, &files);

        let found = snapshot.after();
        assert!(found.is_empty(), "{found:?}");
    }

    /// fails if a build that swaps the link for a file of its own is blamed.
    /// The checkout's copy is untouched and the release is correct.
    #[test]
    fn a_build_that_replaces_the_link_with_its_own_file_is_silent() {
        let (_dir, checkout, release) = linked_release(&["koji"]);
        let files = linked(&["koji"]);
        let snapshot = Snapshot::before("web", &release, &files);

        fs::remove_file(release.join("koji")).expect("remove link");
        fs::write(release.join("koji"), "the build's own").expect("own file");

        let found = snapshot.after();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            fs::read_to_string(checkout.join("koji")).expect("read"),
            "built by the last release"
        );
    }

    #[test]
    fn a_linked_directory_that_gains_an_entry_is_named() {
        let (_dir, checkout, release) = linked_release(&[]);
        fs::create_dir(checkout.join("out")).expect("dir");
        symlink(checkout.join("out"), release.join("out")).expect("link");
        age(&checkout.join("out"));
        let files = linked(&["out"]);
        let snapshot = Snapshot::before("web", &release, &files);

        fs::write(release.join("out/koji"), "built").expect("new entry");

        let found = snapshot.after();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`out`"), "{found:?}");
    }

    /// fails if a linked path that was never readable is an error or a
    /// warning: this check only ever warns.
    #[test]
    fn a_linked_path_that_cannot_be_read_is_skipped() {
        let (_dir, checkout, release) = linked_release(&["live"]);
        symlink(checkout.join("gone"), release.join("dangling")).expect("dangling link");
        let files = linked(&["live", "dangling", "missing"]);
        let snapshot = Snapshot::before("web", &release, &files);

        fs::write(release.join("live"), "rebuilt").expect("write through");
        fs::write(checkout.join("gone"), "now present").expect("appears");

        let found = snapshot.after();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("`live`"), "{found:?}");
    }
}
