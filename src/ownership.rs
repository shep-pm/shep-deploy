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

/// What a visit acts on, always a descriptor the walk already holds.
pub(crate) enum Handle<'a> {
    /// A directory the walk has open.
    Dir(&'a OwnedFd),
    /// A file the walk has open, never a symlink and never shared with another
    /// name.
    File(&'a OwnedFd),
}

/// How an entry that is not a directory is opened: a symlink is refused by
/// the kernel, and `O_NONBLOCK` keeps a FIFO from holding the walk.
const WALK_FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Visits the release root and everything under it that is not a symlink.
///
/// Each directory is opened with `openat` against the descriptor of the one
/// above it, so a directory swapped for a link between the listing and the
/// open is refused by the kernel and skipped, never followed. Symlinks are
/// never visited: the shared ones point into the operator's checkout. A file
/// with another hard link is skipped too, since changing its owner changes
/// the other name's. The `.git` pointer at the release root is left out, which
/// does not stop the build replacing it: see [`pin_git_pointer`].
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
            _ => match rustix::fs::openat(dir, &name, WALK_FILE, Mode::empty()) {
                // A name that shares its inode with another is left alone:
                // the other name may be outside the release, and the owner is
                // the inode's. Checked on the descriptor that is then changed,
                // so the name cannot be swapped between the two.
                Ok(file) => match rustix::fs::fstat(&file) {
                    Ok(stat) if stat.st_nlink > 1 => {}
                    Ok(_) => visit(&below, Handle::File(&file))?,
                    Err(errno) => return Err(io_error(&base.join(&below), errno)),
                },
                // Became a link, went away, or is a socket, which cannot be
                // opened.
                Err(errno)
                    if errno == Errno::LOOP || errno == Errno::NOENT || errno == Errno::NXIO => {}
                Err(errno) => return Err(io_error(&base.join(&below), errno)),
            },
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
/// so is the `.git` pointer at the release root, which the build can still
/// replace because it owns the directory; see [`walk_release`] and
/// [`pin_git_pointer`].
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
            Handle::Dir(fd) | Handle::File(fd) => rustix::fs::fchown(fd, owner, group),
        }
        .map_err(|errno| io_error(&base.join(relative), errno))
    }
}

/// The `.git` pointer file at a release root, as the dog wrote it.
pub(crate) struct GitPointer(Vec<u8>);

const GIT_POINTER: &str = ".git";

/// The longest pointer that is read; a real one is a line naming a directory.
const GIT_POINTER_LIMIT: u64 = 64 * 1024;

/// Reads the release's `.git` pointer so it can be put back after the build.
///
/// The build user owns the release root and so can delete the pointer and
/// write another, which leaving the file itself unchowned does not stop. A
/// pointer re-aimed at a directory git did not make for the release fails
/// git's validation when retention removes the worktree, on every cycle.
///
/// `None` when there is nothing to pin: no pointer, or one that is not a
/// regular file owned by `dog_uid`. The dog wrote the pointer and never hands
/// it over, so any other owner means an earlier build replaced it, and this
/// does not vouch for that one.
///
/// # Errors
/// [`Error::Io`] naming the release if it cannot be opened or the pointer
/// cannot be read.
pub(crate) fn pin_git_pointer(release: &Path, dog_uid: u32) -> Result<Option<GitPointer>, Error> {
    use std::io::Read as _;

    let path = release.join(GIT_POINTER);
    let root = rustix::fs::open(release, WALK_DIRECTORY, Mode::empty())
        .map_err(|errno| io_error(release, errno))?;
    let file = match rustix::fs::openat(&root, GIT_POINTER, WALK_FILE, Mode::empty()) {
        Ok(file) => file,
        Err(errno) if errno == Errno::NOENT || errno == Errno::LOOP || errno == Errno::NXIO => {
            return Ok(None);
        }
        Err(errno) => return Err(io_error(&path, errno)),
    };
    let stat = rustix::fs::fstat(&file).map_err(|errno| io_error(&path, errno))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_uid != dog_uid {
        return Ok(None);
    }
    let mut content = Vec::new();
    std::fs::File::from(file)
        .take(GIT_POINTER_LIMIT)
        .read_to_end(&mut content)
        .map_err(Error::at(&path))?;
    Ok(Some(GitPointer(content)))
}

/// Puts the pinned `.git` pointer back at the release root, whatever the
/// build left at that name.
///
/// The name is unlinked and made again with `O_EXCL | O_NOFOLLOW` against the
/// release's own descriptor, so nothing is written through a link. A process
/// the build left behind outside its group can still race this; it can do no
/// more than the build could.
///
/// # Errors
/// [`Error::Io`] naming the pointer if the name cannot be cleared or made.
pub(crate) fn restore_git_pointer(release: &Path, pointer: &GitPointer) -> Result<(), Error> {
    use std::io::Write as _;

    let path = release.join(GIT_POINTER);
    let root = rustix::fs::open(release, WALK_DIRECTORY, Mode::empty())
        .map_err(|errno| io_error(release, errno))?;
    match rustix::fs::unlinkat(&root, GIT_POINTER, AtFlags::empty()) {
        Ok(()) => {}
        Err(errno) if errno == Errno::NOENT => {}
        Err(errno) => return Err(io_error(&path, errno)),
    }
    let file = rustix::fs::openat(
        &root,
        GIT_POINTER,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .map_err(|errno| io_error(&path, errno))?;
    std::fs::File::from(file)
        .write_all(&pointer.0)
        .map_err(Error::at(&path))
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

    /// fails if a file with more than one link is handed over. A hard link
    /// shares its inode with a name that may live outside the release, and
    /// changing the owner of one changes the owner of both.
    #[test]
    fn a_file_with_another_hard_link_is_never_visited() {
        let outside = fixtures::fixture_release(&[("secret", "")]);
        let release = fixtures::fixture_release(&[("plain.txt", ""), ("twin.txt", "")]);
        fs::hard_link(outside.path().join("secret"), release.path().join("linked")).expect("link");
        fs::hard_link(
            release.path().join("twin.txt"),
            release.path().join("twin-too.txt"),
        )
        .expect("link");

        let paths: Vec<_> = visited(release.path())
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        assert_eq!(paths, ["", "plain.txt"]);
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

    fn my_uid(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(path).expect("stat").uid()
    }

    /// fails if a build that deletes and recreates the `.git` pointer, which
    /// owning the release root allows, leaves the re-aimed one in place.
    #[test]
    fn a_re_aimed_git_pointer_is_put_back_after_the_build() {
        let release =
            fixtures::fixture_release(&[(".git", "gitdir: /srv/tree/git/worktrees/abc\n")]);
        let pointer = pin_git_pointer(release.path(), my_uid(release.path()))
            .expect("pins")
            .expect("a pointer the dog owns");

        fs::remove_file(release.path().join(".git")).expect("the build deletes it");
        fs::write(release.path().join(".git"), "gitdir: /elsewhere\n").expect("and re-aims it");
        restore_git_pointer(release.path(), &pointer).expect("restores");

        assert_eq!(
            fs::read_to_string(release.path().join(".git")).expect("reads"),
            "gitdir: /srv/tree/git/worktrees/abc\n"
        );
    }

    /// fails if the pointer is put back by writing through whatever the build
    /// left at its name.
    #[test]
    fn restoring_the_git_pointer_does_not_write_through_a_link() {
        let release = fixtures::fixture_release(&[(".git", "gitdir: x\n")]);
        let outside = fixtures::fixture_release(&[("victim", "untouched")]);
        let pointer = pin_git_pointer(release.path(), my_uid(release.path()))
            .expect("pins")
            .expect("a pointer the dog owns");

        fs::remove_file(release.path().join(".git")).expect("removed");
        std::os::unix::fs::symlink(outside.path().join("victim"), release.path().join(".git"))
            .expect("link");
        restore_git_pointer(release.path(), &pointer).expect("restores");

        assert_eq!(
            fs::read_to_string(outside.path().join("victim")).expect("reads"),
            "untouched"
        );
        assert_eq!(
            fs::read_to_string(release.path().join(".git")).expect("reads"),
            "gitdir: x\n"
        );
    }

    /// fails if a pointer the dog does not own, or no pointer at all, is
    /// pinned. A previous build that replaced it must not be vouched for.
    #[test]
    fn a_git_pointer_owned_by_somebody_else_is_not_pinned() {
        let release = fixtures::fixture_release(&[(".git", "gitdir: x\n")]);
        let other = my_uid(release.path()).wrapping_add(1);
        assert!(
            pin_git_pointer(release.path(), other)
                .expect("reads")
                .is_none()
        );

        let bare = fixtures::fixture_release(&[]);
        assert!(
            pin_git_pointer(bare.path(), my_uid(bare.path()))
                .expect("reads")
                .is_none()
        );
    }
}
