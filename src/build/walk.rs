//! The handle-based walk both ends of an artifact copy go through.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::error::Error;
use crate::shared::printable;

#[cfg(doc)]
use super::contain::{artifact_roots, lands_within, resolve_deepest};

/// How every directory in a [`Walk`] is opened.
///
/// `O_NOFOLLOW` so a component that is a symlink is refused by the kernel
/// rather than followed, and `O_DIRECTORY` so one that has become a regular
/// file is refused too. Both are decided at open time, which is what makes
/// them different in kind from a check on a name.
const WALK_DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The refusal an end of a copy earns when a name stops meaning what it meant
/// while the copy was using it.
///
/// Deliberately the same sentence for every way that can happen. An operator
/// reading it cannot act on which component moved, and whoever moved it does
/// not need telling how far they got.
pub(super) fn raced(artifact: &Path) -> Error {
    Error::Config(format!(
        "build.artifacts entry `{}` changed underneath the check; refusing to copy it",
        printable(artifact.display())
    ))
}

/// One end of a copy, resolved into the root it lands under and the
/// components that reach it from there.
///
/// Splitting a path that way is what makes a handle-based walk possible.
/// Handed a whole path, the kernel resolves every directory above the last
/// one by name and follows whatever symlink it finds there; `O_NOFOLLOW`
/// governs the last component and nothing above it. Handed a root and a list
/// of names, [`Self::parent`] can open them one at a time against the
/// descriptor of the one before, so nothing above the leaf is ever resolved
/// by name twice and a directory swapped mid-walk is refused instead of
/// followed.
///
/// The resolution done here is a snapshot and is not trusted as one. It picks
/// which names to walk; the walk is what decides where they may go.
pub(super) struct Walk<'a> {
    /// The two directories an artifact may land under, resolved. Both ends of
    /// a copy are confined to these, and a symlinked component is followed
    /// only when it names one of them.
    roots: &'a [PathBuf; 2],
    /// The one of `roots` this end lands under, and where its walk starts.
    root: PathBuf,
    /// The directories to walk through from `root`, in order.
    parents: Vec<OsString>,
    /// The file itself, opened from the descriptor `parents` ends at.
    leaf: OsString,
    /// The entry being copied, for the refusals this reports.
    artifact: &'a Path,
    /// This end's whole path, for the [`Error::Io`] it reports.
    path: PathBuf,
}

impl<'a> Walk<'a> {
    /// Resolves `path` against `roots`, or `None` if it lands under neither.
    ///
    /// The deepest ancestor of `path` that exists is canonicalised, exactly
    /// as [`resolve_deepest`] does it and for the same reason, and whatever
    /// of `path` does not exist yet is appended verbatim. A destination's own
    /// name is normally in that second part, and so are the directories a
    /// redirected build's output tree still needs.
    ///
    /// `None` is not the message an operator reads. [`lands_within`] has
    /// already answered the same question by name, and its answer says which
    /// end resolved where; this one only has to be right.
    pub(super) fn new(roots: &'a [PathBuf; 2], artifact: &'a Path, path: &Path) -> Option<Self> {
        let mut probe = path.to_owned();
        let mut tail = Vec::new();
        let real = loop {
            if let Ok(real) = probe.canonicalize() {
                break real;
            }
            tail.push(probe.file_name()?.to_owned());
            if !probe.pop() {
                return None;
            }
        };
        tail.reverse();

        let root = roots.iter().find(|root| real.starts_with(root))?;
        let mut rest: Vec<OsString> = real
            .strip_prefix(root)
            .ok()?
            .components()
            .map(|component| component.as_os_str().to_owned())
            .collect();
        rest.extend(tail);
        // A path that IS a root has no last component to open. An entry can
        // reach that: `artifacts = ["target"]` under a `CARGO_TARGET_DIR`
        // redirect resolves its source to the redirect itself. `copy_artifact`
        // refuses it by name before either walk is built, so `None` here is a
        // root that appeared underneath one, which is a race like any other.
        let leaf = rest.pop()?;

        Some(Self {
            roots,
            root: root.clone(),
            parents: rest,
            leaf,
            artifact,
            path: path.to_owned(),
        })
    }

    /// Opens the directory [`Self::leaf`] lives in, one component at a time
    /// from `root`'s own descriptor.
    ///
    /// `create` makes the directories a destination needs on the way, with
    /// `mkdirat` on the descriptor the walk has reached rather than
    /// `create_dir_all` on a name.
    pub(super) fn parent(&self, create: bool) -> Result<OwnedFd, Error> {
        let mut dir = self.open_root(&self.root)?;
        for component in &self.parents {
            dir = self.step(&dir, component, create)?;
        }
        Ok(dir)
    }

    /// The file this walk ends at.
    pub(super) fn leaf(&self) -> &OsStr {
        &self.leaf
    }

    /// One component of the walk, opened against the one above it.
    fn step(&self, dir: &OwnedFd, component: &OsStr, create: bool) -> Result<OwnedFd, Error> {
        match rustix::fs::openat(dir, component, WALK_DIRECTORY, Mode::empty()) {
            Ok(fd) => Ok(fd),
            // A symlink where a directory was. `O_NOFOLLOW` answers `ELOOP`
            // for one, and a kernel that got far enough to see it is not a
            // directory answers `ENOTDIR`. Both mean the same thing here, and
            // `follow` puts the kernel's own error back if the component
            // turns out not to be a link at all.
            Err(errno) if errno == Errno::LOOP || errno == Errno::NOTDIR => {
                self.follow(dir, component, errno)
            }
            Err(errno) if errno == Errno::NOENT && create => {
                // `EEXIST` is the build's own output tree, or a concurrent
                // artifact, having got here first. The open below is what
                // decides whether what is there now can be walked through.
                match rustix::fs::mkdirat(dir, component, Mode::from_bits_truncate(0o755)) {
                    Ok(()) => {}
                    Err(errno) if errno == Errno::EXIST => {}
                    Err(errno) => return Err(self.io(errno)),
                }
                rustix::fs::openat(dir, component, WALK_DIRECTORY, Mode::empty())
                    .map_err(|errno| self.refuse(errno))
            }
            Err(errno) => Err(self.io(errno)),
        }
    }

    /// Continues the walk from the root a symlinked component names, or
    /// refuses the component.
    ///
    /// A symlinked component is not wrong by itself, which is the constraint
    /// every earlier attempt at this walk broke: `shared::link_cache` makes
    /// `release/target` a link at the dog's own cache on purpose, so refusing
    /// every one of them refuses the ordinary cargo arrangement.
    ///
    /// So one is followed, and only ever to a root, and the walk then
    /// continues from THAT root's own descriptor rather than from anything
    /// the link resolved to. A link naming a root is worth no more than the
    /// root itself, which is the property that makes reading it safe.
    fn follow(&self, dir: &OwnedFd, component: &OsStr, original: Errno) -> Result<OwnedFd, Error> {
        let Ok(target) = rustix::fs::readlinkat(dir, component, Vec::new()) else {
            // Not a link after all, so `ENOTDIR` came from a regular file or
            // something else that is simply not a directory. The kernel's own
            // answer is the honest one to report.
            return Err(self.io(original));
        };
        let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
        // A relative target is refused rather than resolved. This holds the
        // descriptor of the directory the link sits in and deliberately not
        // that directory's name, so there is nothing to resolve a relative
        // target against, and canonicalising it against this process's own
        // working directory would answer for somewhere else entirely.
        //
        // Nothing legitimate is lost. `shared::link_cache` writes an absolute
        // target, and a link that is already there when the path is resolved
        // is followed by `Walk::new` whatever its spelling. What reaches here
        // is a component that became a link while the walk ran.
        let named = target
            .canonicalize()
            .ok()
            .filter(|_| target.is_absolute())
            .filter(|real| self.roots.iter().any(|root| root == real));
        match named {
            Some(real) => self.open_root(&real),
            None => Err(raced(self.artifact)),
        }
    }

    /// Opens one of the two roots, the only directory a walk resolves by name.
    ///
    /// Safe to do by name because [`artifact_roots`] canonicalised both, so
    /// neither ends in a symlink, and because a root is where the walk's
    /// trust comes from rather than something it derives.
    fn open_root(&self, root: &Path) -> Result<OwnedFd, Error> {
        rustix::fs::open(root, WALK_DIRECTORY, Mode::empty()).map_err(|errno| self.io(errno))
    }

    /// The kernel's own answer, against this end's path.
    fn io(&self, errno: Errno) -> Error {
        Error::Io {
            path: self.path.clone(),
            source: io::Error::from(errno),
        }
    }

    /// Whichever of the two a failed open deserves: a refusal when it failed
    /// because something moved, the kernel's answer otherwise.
    fn refuse(&self, errno: Errno) -> Error {
        if errno == Errno::LOOP || errno == Errno::NOTDIR {
            raced(self.artifact)
        } else {
            self.io(errno)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::artifacts::copy_artifact;
    use super::super::contain::artifact_roots;
    use super::*;
    use crate::fixtures;

    /// fails if a component that becomes a symlink to one of the two roots
    /// stops being followed.
    ///
    /// This is constraint 1 in `docs/specs/deferred.md`, and the one every
    /// earlier attempt at this walk broke: `shared::link_cache` makes
    /// `release/target` a symlink at the dog's own build cache on purpose, so
    /// a walk that refuses every symlinked component refuses the ordinary
    /// cargo arrangement.
    ///
    /// Deterministic, where the racing test above is not: `Walk::new` reads
    /// the path BEFORE the link exists and the walk runs after, which is the
    /// same order the racer produces and needs no thread to arrange.
    #[test]
    fn a_component_that_becomes_a_link_to_a_root_is_followed_to_that_root() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let roots = artifact_roots(release.path(), cache.path());
        let artifact = PathBuf::from("target/koji");
        let to = release.path().join(&artifact);

        let walk = Walk::new(&roots, &artifact, &to).expect("the destination lands in the release");
        std::os::unix::fs::symlink(cache.path(), release.path().join("target")).expect("link");

        let dir = walk
            .parent(false)
            .expect("a link naming a root is followed");
        rustix::fs::openat(
            &dir,
            walk.leaf(),
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
            Mode::from_bits_truncate(0o600),
        )
        .expect("the walk ends somewhere writable");
        assert!(
            cache.path().join("koji").exists(),
            "the walk must continue from the root the link named, not from the link"
        );
    }

    /// fails if a component that becomes a symlink to anywhere else is
    /// followed.
    ///
    /// The twin of the test above and the reason it is not simply "follow
    /// symlinks": a link is worth exactly what it names, and anything that is
    /// not one of the two roots is refused whether it existed when the path
    /// was resolved or appeared afterwards.
    #[test]
    fn a_component_that_becomes_a_link_to_anywhere_else_is_refused() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let outside = fixtures::tempdir();
        let roots = artifact_roots(release.path(), cache.path());
        let artifact = PathBuf::from("target/koji");
        let to = release.path().join(&artifact);

        let walk = Walk::new(&roots, &artifact, &to).expect("the destination lands in the release");
        std::os::unix::fs::symlink(outside.path(), release.path().join("target")).expect("link");

        let err = walk
            .parent(false)
            .expect_err("a link naming neither root must be refused");
        assert!(
            format!("{err}").contains("changed underneath the check"),
            "must say the name stopped meaning what it meant: {err}"
        );
        assert!(
            !outside.path().join("koji").exists(),
            "nothing may be created outside both roots"
        );
    }

    /// fails if a symlinked component whose target is relative is followed.
    ///
    /// `Walk::follow` reads a component with `readlinkat` and holds only the
    /// descriptor of the directory it sits in, never that directory's name. A
    /// relative target cannot be resolved from a descriptor without one, and
    /// canonicalising it against this process's own working directory would
    /// answer for a completely different place.
    ///
    /// So a relative target is refused, and this pins it. Nothing legitimate
    /// is lost: `shared::link_cache` writes an absolute target, and a link
    /// that is already there when the path is resolved is followed by
    /// `Walk::new` whatever its spelling.
    #[test]
    fn a_component_that_becomes_a_relative_link_is_refused() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let roots = artifact_roots(release.path(), cache.path());
        let artifact = PathBuf::from("target/koji");
        let to = release.path().join(&artifact);

        let walk = Walk::new(&roots, &artifact, &to).expect("the destination lands in the release");
        // The two fixtures are siblings, so this names the cache root exactly
        // as a relative path from the release. It is still refused.
        let sibling = Path::new("..").join(cache.path().file_name().expect("a name"));
        std::os::unix::fs::symlink(&sibling, release.path().join("target")).expect("link");

        let err = walk
            .parent(false)
            .expect_err("a relative link target must be refused");
        assert!(
            format!("{err}").contains("changed underneath the check"),
            "must be the refusal, not the kernel's own error: {err}"
        );
    }

    /// fails if a directory swapped for a symlink WHILE the copy runs can
    /// carry the write outside the release and the cache.
    ///
    /// The gap this closes is the one `docs/specs/deferred.md` carried through
    /// four review rounds: every containment check resolves a name, and
    /// `O_NOFOLLOW` binds only the last component, so a build's backgrounded
    /// job can swap a DIRECTORY above the destination between the check and
    /// the open, and the kernel follows it.
    ///
    /// A loop against a thread doing nothing but flipping that one component,
    /// because a single attempt proves nothing either way. Two thousand
    /// attempts, measured: a walk that follows a swapped parent loses within
    /// the first few of them, in a hundredth of a second. Nobody won this
    /// race by hand in round 7, which is what made it read as theatre.
    ///
    /// What is asserted is the invariant rather than the outcome of any one
    /// attempt. A copy may succeed and a copy may be refused, but nothing may
    /// ever appear outside the two roots.
    #[test]
    fn a_parent_swapped_for_a_symlink_during_the_copy_cannot_carry_a_write_out() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let outside = fixtures::tempdir();

        std::fs::create_dir_all(cache.path().join("dist")).expect("build output dir");
        std::fs::write(cache.path().join("dist/app.js"), b"ARTIFACT").expect("build output");

        let roots = artifact_roots(release.path(), cache.path());
        let env: BTreeMap<String, String> = [(
            "CARGO_TARGET_DIR".to_owned(),
            cache.path().display().to_string(),
        )]
        .into();
        // `target/` is stripped and rejoined onto `CARGO_TARGET_DIR`, so the
        // source is `<cache>/dist/app.js` and the destination
        // `<release>/target/dist/app.js`. `dist` is the component the racer
        // swaps.
        let artifact = PathBuf::from("target/dist/app.js");

        let swapped = release.path().join("target/dist");
        std::fs::create_dir_all(&swapped).expect("the destination's parent");

        let stop = Arc::new(AtomicBool::new(false));
        let racer = {
            let (stop, swapped, elsewhere) = (
                Arc::clone(&stop),
                swapped.clone(),
                outside.path().to_owned(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_dir_all(&swapped);
                    let _ = std::os::unix::fs::symlink(&elsewhere, &swapped);
                    let _ = std::fs::remove_file(&swapped);
                    let _ = std::fs::create_dir_all(&swapped);
                }
            })
        };

        let escaped = outside.path().join("app.js");
        let mut leaked = None;
        let mut copied = 0_u32;
        for _ in 0..2_000 {
            if copy_artifact(release.path(), &roots, &env, &artifact).is_ok() {
                copied += 1;
            }
            if escaped.exists() {
                leaked = Some(std::fs::read(&escaped).unwrap_or_default());
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        racer.join().expect("the racer");

        assert!(
            leaked.is_none(),
            "a swapped parent carried the copy outside both roots: {:?}",
            leaked.as_deref().map(String::from_utf8_lossy)
        );
        // The positive control. `leaked.is_none()` holds just as well if every
        // attempt was refused for a reason that has nothing to do with a
        // swapped parent, and a `Walk::new` that answered `None` every time
        // would keep this green while proving nothing.
        assert!(
            copied > 0,
            "no attempt copied anything, so the invariant above proves nothing"
        );
    }
}
