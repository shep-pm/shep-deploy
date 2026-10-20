//! Making a symlink in a release one directory handle at a time.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// How each directory above the link is opened: a symlink is refused by the
/// kernel rather than followed.
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Makes `release/relative` a symlink to `target`, creating the directories
/// above it, and answers whether it is now in place: `false` when something
/// else is already there that is not a link to `target`.
///
/// Every directory is opened against the one above it with `O_NOFOLLOW`, so
/// a component the previous build swapped for a link is refused rather than
/// followed. The dog runs as root and a retried release belongs to the build
/// user, so a path-based `create_dir_all` and `symlink` here would write
/// wherever that link pointed.
///
/// # Errors
/// The kernel's answer, if a directory cannot be opened or made, or the link
/// cannot be made for a reason other than something already being there.
pub(super) fn link_within(release: &Path, relative: &Path, target: &Path) -> io::Result<bool> {
    let names: Vec<_> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let Some((leaf, parents)) = names.split_last() else {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    };

    let mut dir: OwnedFd = rustix::fs::open(release, DIRECTORY, Mode::empty())?;
    for name in parents {
        dir = match rustix::fs::openat(&dir, *name, DIRECTORY, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::NOENT) => {
                match rustix::fs::mkdirat(&dir, *name, Mode::from_bits_truncate(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(errno) => return Err(errno.into()),
                }
                rustix::fs::openat(&dir, *name, DIRECTORY, Mode::empty())?
            }
            Err(errno) => return Err(errno.into()),
        };
    }

    match rustix::fs::symlinkat(target, &dir, *leaf) {
        Ok(()) => Ok(true),
        Err(Errno::EXIST) => Ok(rustix::fs::readlinkat(&dir, *leaf, Vec::new())
            .is_ok_and(|existing| existing.as_bytes() == target.as_os_str().as_encoded_bytes())),
        Err(errno) => Err(errno.into()),
    }
}
