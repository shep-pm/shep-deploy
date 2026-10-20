//! What a root shepherd hands to the build's user, and the one failure that
//! means the user could not be given enough.
//!
//! The build runs as `user` in a release the dog created as root, so the
//! release has to belong to that user for the build to write in it. Only the
//! release and the build cache directory change hands; everything else in the
//! tree stays root's. See [`hand_over`].

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Gid, Mode, OFlags, Uid};
use rustix::io::Errno;

use crate::error::Error;
use crate::shared::printable;

/// How every directory of the walk is opened: a symlink is refused by the
/// kernel rather than followed, and so is anything that is not a directory.
const WALK_DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// What a visit acts on, always by a descriptor the walk already holds.
pub(crate) enum Handle<'a> {
    /// A directory the walk has open.
    Dir(&'a OwnedFd),
    /// An entry that is not a directory, named in the directory `parent`.
    Entry {
        /// The open directory the entry lives in.
        parent: &'a OwnedFd,
        /// The entry's name within `parent`.
        name: &'a OsStr,
    },
}

/// Visits the release root and everything under it that is not a symlink.
///
/// Each directory is opened with `openat` against the descriptor of the one
/// above it, so a directory swapped for a link between the listing and the
/// open is refused by the kernel and skipped, never followed. Symlinks are
/// never visited: the shared ones point into the operator's checkout. The
/// `.git` pointer at the release root is left out so the build cannot re-aim
/// it at a repository of its own.
///
/// `visit` gets the path relative to `release`, empty for the root itself.
fn walk_release(
    release: &Path,
    visit: &mut impl FnMut(&Path, Handle<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let root = rustix::fs::open(release, WALK_DIRECTORY, Mode::empty())
        .map_err(|errno| io_error(release, errno))?;
    walk_dir(release, Path::new(""), &root, visit)
}

fn walk_dir(
    base: &Path,
    relative: &Path,
    dir: &OwnedFd,
    visit: &mut impl FnMut(&Path, Handle<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    visit(relative, Handle::Dir(dir))?;

    let here = base.join(relative);
    let mut listing = Dir::read_from(dir).map_err(|errno| io_error(&here, errno))?;
    let mut entries = Vec::new();
    while let Some(entry) = listing.read() {
        let entry = entry.map_err(|errno| io_error(&here, errno))?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes()).to_owned();
        if name == "." || name == ".." {
            continue;
        }
        entries.push((name, entry.file_type()));
    }

    for (name, kind) in entries {
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let below = relative.join(&name);
        // Some filesystems list no type; ask for it without following a link.
        let kind = if kind == FileType::Unknown {
            rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_or(FileType::Unknown, |stat| {
                    FileType::from_raw_mode(stat.st_mode)
                })
        } else {
            kind
        };
        match kind {
            FileType::Symlink => {}
            FileType::Directory => {
                match rustix::fs::openat(dir, &name, WALK_DIRECTORY, Mode::empty()) {
                    Ok(child) => walk_dir(base, &below, &child, visit)?,
                    // Became a link, or went away, after it was listed.
                    Err(errno)
                        if errno == Errno::LOOP
                            || errno == Errno::NOTDIR
                            || errno == Errno::NOENT => {}
                    Err(errno) => return Err(io_error(&base.join(&below), errno)),
                }
            }
            _ => visit(
                &below,
                Handle::Entry {
                    parent: dir,
                    name: &name,
                },
            )?,
        }
    }
    Ok(())
}

/// Visits the build cache directory itself and nothing under it.
///
/// An absent directory is nothing to hand over: the cache is made when the
/// release is staged, and a release that brought its own `target` has no use
/// for it.
fn visit_cache_target(
    cache_target: &Path,
    visit: &mut impl FnMut(&Path, Handle<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    match rustix::fs::open(cache_target, WALK_DIRECTORY, Mode::empty()) {
        Ok(dir) => visit(Path::new(""), Handle::Dir(&dir)),
        Err(errno) if errno == Errno::NOENT => Ok(()),
        Err(errno) => Err(io_error(cache_target, errno)),
    }
}

/// Whether a build as `uid` needs the release handed over: this process is
/// root, which is the only uid that can give anything away, and `uid` is not
/// root, which would own it already.
pub(crate) fn is_due(euid: Option<u32>, uid: u32) -> bool {
    euid == Some(0) && uid != 0
}

/// Gives `uid` and `gid` the release, the files and directories in it, and
/// the build cache directory `cache_target`, and nothing else.
///
/// The release root is created by the dog, so the build user cannot write in
/// it until it is handed over. Each entry is changed through a descriptor the
/// walk holds and never through a path: on a retried release, a process the
/// previous build left behind can swap a directory for a link between two
/// steps, and a path chown by root would follow it. Symlinks are skipped, and
/// so is the `.git` pointer at the release root; see [`walk_release`].
/// `cache_target` is changed itself and not what is in it.
///
/// Everything else in the tree, including `releases/` above the release,
/// stays the dog's.
///
/// # Errors
/// [`Error::Config`] if `uid` or `gid` is `u32::MAX`, which is no account's.
/// [`Error::Io`] naming the path that failed if the release cannot be
/// opened, listed or walked, or the kernel refuses a change of owner.
pub(crate) fn hand_over(
    release: &Path,
    cache_target: &Path,
    uid: u32,
    gid: u32,
) -> Result<(), Error> {
    if uid == u32::MAX || gid == u32::MAX {
        return Err(Error::Config(format!(
            "uid {uid} and gid {gid} cannot be handed a release: (uint32_t)-1 names no account"
        )));
    }
    walk_release(release, &mut chown_under(release, uid, gid))?;
    visit_cache_target(cache_target, &mut chown_under(cache_target, uid, gid))
}

/// A visit that changes the owner of what it reaches, naming `base` joined
/// with the visited path in the error.
fn chown_under(
    base: &Path,
    uid: u32,
    gid: u32,
) -> impl FnMut(&Path, Handle<'_>) -> Result<(), Error> {
    let (owner, group) = (Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)));
    move |relative, handle| {
        match handle {
            Handle::Dir(dir) => rustix::fs::fchown(dir, owner, group),
            Handle::Entry { parent, name } => {
                rustix::fs::chownat(parent, name, owner, group, AtFlags::SYMLINK_NOFOLLOW)
            }
        }
        .map_err(|errno| io_error(&base.join(relative), errno))
    }
}

/// Rewords a build that could not even start as `user`, when the cause is
/// that `user` was refused somewhere on the way to the release.
///
/// The child changes into the release after it has dropped to `user`, so
/// every directory above the release needs search permission for `user`, and
/// a `$SHEP_HOME` of mode 700 has none. The kernel's answer, `Permission
/// denied (os error 13)`, names the release and not the directory that said
/// it. Any other error, and a refusal with no `user` set, comes back as it
/// was.
pub(crate) fn explain_spawn_failure(error: Error, user: Option<&str>, release: &Path) -> Error {
    match (error, user) {
        (Error::Io { source, .. }, Some(user))
            if source.kind() == io::ErrorKind::PermissionDenied =>
        {
            Error::Config(format!(
                "the build could not start as `user = {user:?}`: {user} was refused on the way to \
                 {}. That user needs search permission on every directory down to the release. \
                 Either put `$SHEP_HOME` somewhere {user} can search, or run the shepherd as that \
                 user",
                printable(release.display())
            ))
        }
        (error, _) => error,
    }
}

fn io_error(path: &Path, errno: Errno) -> Error {
    Error::Io {
        path: path.to_owned(),
        source: io::Error::from(errno),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::fixtures;

    fn visited(release: &Path) -> Vec<PathBuf> {
        let mut seen = Vec::new();
        walk_release(release, &mut |relative, _| {
            seen.push(relative.to_owned());
            Ok(())
        })
        .expect("walks");
        seen.sort();
        seen
    }

    /// fails if the walk misses a file or a directory of the release.
    #[test]
    fn the_walk_reaches_every_file_and_directory_of_the_release() {
        let release = fixtures::fixture_release(&[
            ("Flockfile.toml", ""),
            ("src/main.rs", ""),
            ("src/deep/er/lib.rs", ""),
        ]);

        let paths: Vec<_> = visited(release.path())
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        assert_eq!(
            paths,
            [
                "",
                "Flockfile.toml",
                "src",
                "src/deep",
                "src/deep/er",
                "src/deep/er/lib.rs",
                "src/main.rs",
            ]
        );
    }

    /// fails if a symlink, or anything behind one, is reached. The shared
    /// links point into the operator's checkout, and chowning through one
    /// would give the build user the operator's `.env`.
    #[test]
    fn a_symlink_is_never_visited_or_followed() {
        let outside = fixtures::fixture_release(&[("secret/.env", ""), ("loose.txt", "")]);
        let release = fixtures::fixture_release(&[("src/main.rs", "")]);
        std::os::unix::fs::symlink(outside.path().join("secret"), release.path().join("shared"))
            .expect("link to a directory");
        std::os::unix::fs::symlink(
            outside.path().join("loose.txt"),
            release.path().join("loose"),
        )
        .expect("link to a file");
        std::os::unix::fs::symlink("src/main.rs", release.path().join("src/alias"))
            .expect("link inside the release");

        let paths: Vec<_> = visited(release.path())
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        assert_eq!(paths, ["", "src", "src/main.rs"]);
    }

    /// fails if the `.git` pointer at the release root is handed over. A
    /// build that owned it could re-aim it at a repository of its own.
    #[test]
    fn the_git_pointer_at_the_release_root_is_left_alone() {
        let release = fixtures::fixture_release(&[
            (".git", "gitdir: /srv/tree/git/worktrees/abc\n"),
            ("vendor/.git", "a nested one is the build's to own"),
        ]);

        let paths: Vec<_> = visited(release.path())
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        assert_eq!(paths, ["", "vendor", "vendor/.git"]);
    }

    /// fails if the cache directory is handed over recursively. Only the
    /// directory the release links in as `target` changes hands.
    #[test]
    fn only_the_cache_target_itself_is_handed_over() {
        let cache = fixtures::fixture_release(&[("release/koji", ""), ("debug/deps/a.o", "")]);

        let mut seen = Vec::new();
        visit_cache_target(cache.path(), &mut |relative, handle| {
            seen.push((relative.to_owned(), matches!(handle, Handle::Dir(_))));
            Ok(())
        })
        .expect("visits");

        assert_eq!(seen, [(PathBuf::new(), true)]);
    }

    fn denied() -> Error {
        Error::Io {
            path: PathBuf::from("/srv/tree/releases/abc"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        }
    }

    /// fails if a refused spawn as a user stops saying who could not get
    /// where. The bare `Permission denied (os error 13)` it replaces names a
    /// release, which is not the directory the user was refused at.
    #[test]
    fn a_permission_failure_with_a_user_names_the_user_and_the_release() {
        let release = Path::new("/srv/tree/releases/abc");

        let Error::Config(message) = explain_spawn_failure(denied(), Some("reactmap"), release)
        else {
            panic!("a refused spawn as a user is a configuration problem");
        };

        assert!(message.contains("reactmap"), "{message}");
        assert!(message.contains("/srv/tree/releases/abc"), "{message}");
        assert!(message.contains("search permission"), "{message}");
        assert!(message.contains("$SHEP_HOME"), "{message}");
        assert!(message.contains("as that user"), "{message}");
    }

    /// fails if a spawn failure that is not a refusal, or is a refusal with
    /// no `user` to blame, is reworded into the user advice.
    #[test]
    fn any_other_spawn_failure_is_left_as_it_was() {
        let release = Path::new("/srv/tree/releases/abc");

        let no_user = explain_spawn_failure(denied(), None, release);
        assert!(matches!(&no_user, Error::Io { source, .. }
            if source.kind() == io::ErrorKind::PermissionDenied));

        let missing = Error::Io {
            path: release.to_owned(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        let kept = explain_spawn_failure(missing, Some("reactmap"), release);
        assert!(matches!(&kept, Error::Io { source, .. }
            if source.kind() == io::ErrorKind::NotFound));

        let build =
            explain_spawn_failure(Error::Build { status: Some(1) }, Some("reactmap"), release);
        assert!(matches!(build, Error::Build { status: Some(1) }));
    }

    /// fails if a release is handed over by a dog that cannot do it, or to
    /// root, which would change nothing.
    #[test]
    fn only_a_root_dog_building_as_somebody_else_hands_over() {
        assert!(is_due(Some(0), 1001));
        assert!(!is_due(Some(0), 0));
        assert!(!is_due(Some(501), 1001));
        assert!(!is_due(None, 1001));
    }

    /// fails if the real walk stops completing against a live filesystem.
    /// A change of owner to one's own account needs no privilege, so this
    /// runs the whole of `hand_over` without being root.
    #[test]
    fn handing_a_release_to_its_own_owner_succeeds_and_skips_links() {
        use std::os::unix::fs::MetadataExt as _;

        let release = fixtures::fixture_release(&[("src/main.rs", ""), (".git", "gitdir: x")]);
        let cache = fixtures::tempdir();
        std::os::unix::fs::symlink(cache.path(), release.path().join("target")).expect("link");
        let me = fs::metadata(release.path()).expect("stat");

        hand_over(release.path(), cache.path(), me.uid(), me.gid()).expect("hands over");
        assert_eq!(
            fs::metadata(release.path().join("src/main.rs"))
                .expect("stat")
                .uid(),
            me.uid()
        );

        let invalid = hand_over(release.path(), cache.path(), u32::MAX, me.gid());
        assert!(matches!(invalid, Err(Error::Config(_))));
    }
}
