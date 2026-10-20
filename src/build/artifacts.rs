//! Copying a declared artifact from wherever the build left it into the
//! release, as the dog and not as the build.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::error::Error;
use crate::shared::printable;

use super::contain::{lands_within, resolve_deepest};
#[cfg(doc)]
use super::contained_artifacts;
use super::walk::{Walk, raced};

/// Where `artifact` actually landed after the build, given the environment
/// it ran with.
///
/// A path beginning `target/` is resolved against `CARGO_TARGET_DIR`
/// instead, if the build set one: `build.env` exists for exactly this
/// case, and a shared target dir works by moving cargo's entire output
/// tree outside the release, so `target/release/koji` never actually
/// exists at that path on disk - the real file sits at
/// `$CARGO_TARGET_DIR/release/koji`. Every other artifact path is assumed
/// to already sit inside `release` at the path it names, which is the
/// ordinary case for a build with no such redirect.
///
/// Scoped to `CARGO_TARGET_DIR` specifically, not generalised to "any env
/// var that looks like a directory": it is the one redirect this crate's
/// design actually names, and guessing at others would invent behaviour
/// nothing has asked for yet.
pub(super) fn artifact_source(
    release: &Path,
    env: &BTreeMap<String, String>,
    artifact: &Path,
) -> PathBuf {
    if let Some(target_dir) = env.get("CARGO_TARGET_DIR")
        && let Ok(rest) = artifact.strip_prefix("target")
    {
        return Path::new(target_dir).join(rest);
    }
    release.join(artifact)
}

/// Copies one declared artifact from wherever the build actually left it
/// into its named path inside `release`.
///
/// A no-op when the source and destination are already the same path -
/// the ordinary case when no `CARGO_TARGET_DIR` redirect is in play - and
/// that check is not a cheap-exit nicety: verified empirically, `fs::copy`
/// truncates a file to empty when its source and destination are the same
/// path, because it opens the destination for writing before it reads the
/// source. Skipping unconditionally would corrupt exactly the artifacts
/// this function exists to preserve.
///
/// Both ends are opened through [`Walk`], a per-component walk from one of
/// the two roots that never resolves a directory by name twice. That is what
/// bounds the copy, and it is a different thing from the `lands_within` pair
/// that runs first: those answer where a name resolves, and produce the
/// message an operator reads. The walk answers where a handle goes, and can
/// only refuse.
///
/// # Errors
/// [`Error::Config`], naming the entry, in four cases: `artifact` is absolute
/// or contains `..` (see [`contained_artifacts`] for why that is a security
/// boundary); either end resolves outside the release and the build cache;
/// the walk found a component that moved while it ran; or the build never
/// produced it at the path declared. Three more name a file that cannot be
/// one: the entry names the release or the build cache itself, its source is
/// not a regular file, or its destination exists and is not one, which is
/// refused rather than replaced. [`Error::Io`] naming the end being
/// walked if a directory on the way to it cannot be opened or created, the
/// destination if it cannot be opened or have its permissions set, and the
/// source in every other case - including a write to the destination that
/// fails. `io::copy` does not report which side of a copy an error came from,
/// so the one path it can name is the artifact that could not be copied. Read
/// `No space left on device` against a source path as the release's
/// filesystem rather than the cache's.
pub(super) fn copy_artifact(
    release: &Path,
    roots: &[PathBuf; 2],
    env: &BTreeMap<String, String>,
    artifact: &Path,
) -> Result<(), Error> {
    // Checked again here, not only in `contained_artifacts`, because a
    // `BuildSpec` can be built in code without going through the parser -
    // every test in this module does exactly that. Placed BEFORE the walk
    // below so a refused artifact cannot leave directories behind on its way
    // out, and before the `from == to` guard because that guard is what hides
    // the escape in the no-`CARGO_TARGET_DIR` case.
    if artifact.is_absolute()
        || artifact
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(Error::Config(format!(
            "build.artifacts entry `{}` would name a path outside the release",
            printable(artifact.display())
        )));
    }

    let from = artifact_source(release, env, artifact);
    let to = release.join(artifact);

    // Both ends checked against where they REALLY land, not against how they
    // are spelled. The lexical refusal above catches `..` and an absolute
    // path; it cannot catch a committed symlink, and it says nothing at all
    // about the source, which `CARGO_TARGET_DIR` points wherever the
    // repository's own Flockfile says. See `artifact_roots` for what the
    // two roots are and the escapes they were measured against.
    for (end, path) in [("destination", &to), ("source", &from)] {
        if !lands_within(roots, path) {
            return Err(Error::Config(format!(
                "build.artifacts entry `{}` resolves its {end} to `{}`, which is \
                 outside the release and its build cache",
                printable(artifact.display()),
                resolve_deepest(path)
                    .unwrap_or_else(|| path.clone())
                    .display()
            )));
        }

        // An entry can name a root outright. `artifact_source` strips
        // `target` and rejoins the rest onto `CARGO_TARGET_DIR`, so
        // `artifacts = ["target"]` under a redirect resolves its source to
        // the redirect itself, and its destination through the link
        // `link_cache` leaves at `release/target`. Both are the cache.
        //
        // Said here rather than left to `Walk`, which has no last component
        // to open in that case and can only report it as a component that
        // moved. Nothing moved, and an operator told otherwise goes looking
        // for a build job that does not exist.
        // The path itself, not its deepest existing ancestor: a destination
        // that does not exist yet has one, and it is normally the release.
        if path.canonicalize().is_ok_and(|real| roots.contains(&real)) {
            return Err(Error::Config(format!(
                "build.artifacts entry `{}` names the release or its build cache as its \
                 {end}, which is a directory rather than a file the build produced",
                printable(artifact.display())
            )));
        }
    }

    if from == to {
        // Already where it belongs, which is the ordinary case with no
        // `CARGO_TARGET_DIR` redirect: the build wrote it into the release
        // directly and there is nothing to copy.
        //
        // Checked rather than assumed, because this branch is reached for
        // EVERY declared artifact when no redirect is in play, and returning
        // `Ok` without looking makes an artifact that was never produced
        // indistinguishable from one that was. A build that exits 0 without
        // writing what it said it would, or a typo in `artifacts`, then rides
        // through the swap and the reload and surfaces as a runtime failure in
        // a release the operator has been told is deployed.
        if !to.exists() {
            return Err(Error::Config(format!(
                "build.artifacts names `{}`, which the build did not leave there; \
                 either the command did not produce it or the entry is wrong",
                printable(artifact.display())
            )));
        }
        return Ok(());
    }

    // Both ends are opened through a walk that starts at one of the two roots
    // and never resolves a directory by name again. That is the whole fix for
    // the race this function spent four review rounds narrowing: every check
    // above resolves a NAME, and a build's backgrounded job can swap a
    // directory above either end between a name being approved and the same
    // name being followed. `O_NOFOLLOW` does not help, because the kernel
    // honours it on the last component and follows every directory above it.
    //
    // The walk cannot produce the message an operator reads, because by the
    // time it refuses, the thing it refused is usually no longer there to be
    // named. That is what the `lands_within` pair above is for: it answers
    // the same question by name, first, and says which end resolved where.
    let (Some(source_walk), Some(dest_walk)) = (
        Walk::new(roots, artifact, &from),
        Walk::new(roots, artifact, &to),
    ) else {
        return Err(raced(artifact));
    };

    let source_dir = source_walk.parent(false)?;
    // `O_NONBLOCK` because a build can leave a named pipe at a declared
    // artifact path, and opening one for reading waits for a writer. The type
    // check below runs on the handle, so it never got the chance, and nothing
    // above bounds this: the build's own budget is spent before any artifact
    // is copied, and its process group is killed. A build that ran
    // `mkfifo target/koji` and exited 0 left the dog waiting forever for a
    // writer that was already dead, with the tree lock held.
    //
    // It changes nothing for a regular file, which is the only thing this
    // copies. Both kernels ignore the flag there.
    let source = rustix::fs::openat(
        &source_dir,
        source_walk.leaf(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| {
        // `ELOOP` is the source itself being a symlink, which the walk
        // refuses at the leaf the same way it refuses one in the middle. A
        // build's output is a file; a link where one should be is either the
        // swap this walk exists for or a build declaring something it did not
        // produce, and neither is worth following.
        if errno == Errno::LOOP {
            raced(artifact)
        } else {
            Error::Io {
                path: from.clone(),
                source: io::Error::from(errno),
            }
        }
    })?;
    let mut source = fs::File::from(source);
    let opened = source.metadata().map_err(Error::at(&from))?;

    // Refused here, before the destination is walked, because opening a
    // directory read-only succeeds and nothing else would have caught it:
    // `artifacts = ["target/dist"]` under a redirect walked the destination,
    // made its parents, created the destination itself, and only then failed
    // in `io::copy` with `EISDIR` against the source. The build fails either
    // way. What it left behind was an empty file in the release and a
    // directory tree made for it.
    //
    // Same reasoning as the lexical check at the top of this function: a
    // refusal that can happen before anything is created should.
    if !opened.file_type().is_file() {
        return Err(Error::Config(format!(
            "build.artifacts entry `{}` names a source that is not a regular file; \
             artifacts are copied one file at a time",
            printable(artifact.display())
        )));
    }

    // Identity, not spelling. `link_cache` symlinks `release/target` at the
    // dog's own cache, so a redirect through `CARGO_TARGET_DIR` gives a source
    // and a destination that differ as strings and name one file. The lexical
    // `from == to` above cannot see that, and opening the destination for
    // writing truncates the source before anything is read.
    //
    // The original `fs::copy` version carried a comment warning about exactly
    // this and guarded it lexically, which held only while `fs::copy` was the
    // route. Replacing it with an explicit open reopened the hole, so the
    // guard is now on the same footing as the source check above it.
    //
    // Still by name rather than through the walk, and safe that way round: a
    // wrong answer here can only skip a copy, never redirect one. The open
    // below is what decides where anything is written.
    if fs::metadata(&to)
        .is_ok_and(|there| there.dev() == opened.dev() && there.ino() == opened.ino())
    {
        return Ok(());
    }

    let dest_dir = dest_walk.parent(true)?;
    let leaf = dest_walk.leaf();

    // Unlinked first, then created fresh with `O_EXCL`, and the two together
    // are the fix for a hard link. The checks above resolve names and compare
    // inodes, and a hard link has neither a name to resolve nor an inode of
    // its own: `release/dist/app.js` linked by the build to a file elsewhere
    // on the same filesystem passes every one of them, and opening it for
    // writing then truncates and rewrites the far file at the dog's uid.
    // Unlinking takes the link away without touching what it pointed at, and
    // `O_EXCL` refuses to open anything that appeared in between.
    //
    // Only a regular file is unlinked. A directory, a symlink or anything
    // else at the destination is not an artifact the build left, and
    // unlinking it would take the release's own `target -> cache` link with
    // it when `artifacts` names `target` itself, so those are refused.
    //
    // Both syscalls take the walked descriptor rather than the path, so the
    // window that used to sit between this look, the unlink and the open is
    // now a window in which the name can change but the directory it is
    // looked up in cannot.
    match rustix::fs::statat(&dest_dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(there) if FileType::from_raw_mode(there.st_mode) != FileType::RegularFile => {
            return Err(Error::Config(format!(
                "build.artifacts entry `{}` names a destination that is not a regular file; \
                 refusing to replace it",
                printable(artifact.display())
            )));
        }
        Ok(_) => match rustix::fs::unlinkat(&dest_dir, leaf, AtFlags::empty()) {
            Ok(()) => {}
            // Gone between the look and the unlink, and nothing to remove is
            // not a failure.
            Err(errno) if errno == Errno::NOENT => {}
            Err(errno) => {
                return Err(Error::Io {
                    path: to.clone(),
                    source: io::Error::from(errno),
                });
            }
        },
        Err(errno) if errno == Errno::NOENT => {}
        Err(errno) => {
            return Err(Error::Io {
                path: to.clone(),
                source: io::Error::from(errno),
            });
        }
    }

    // Created 0o600 and widened afterwards, so nothing can open the
    // destination while a build's output is still going into it.
    let sink = rustix::fs::openat(
        &dest_dir,
        leaf,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|errno| {
        // Something reappeared at the name between the unlink and the open. A
        // backgrounded build job is the only thing that could, and the copy is
        // refused rather than written through it. Said as a refusal, because
        // the raw `File exists` or `Too many levels of symbolic links` reads
        // as a broken host.
        if errno == Errno::EXIST || errno == Errno::LOOP {
            Error::Config(format!(
                "build.artifacts entry `{}` had its destination replaced underneath the \
                 copy; refusing to write through it",
                printable(artifact.display())
            ))
        } else {
            Error::Io {
                path: to.clone(),
                source: io::Error::from(errno),
            }
        }
    })?;
    let mut sink = fs::File::from(sink);
    io::copy(&mut source, &mut sink).map_err(Error::at(&from))?;

    // `fs::copy` carried the source's mode and this does not: a file created
    // by an explicit open gets the mode it was created with, so 0755 would
    // arrive as 0600 and the executable bit would be gone. That was silent and
    // total for the arrangement this whole function exists to serve.
    // `CARGO_TARGET_DIR` points at the dog's cache, cargo builds the binary
    // there, `copy_artifact` brings it back so `script = "./target/release/koji"`
    // resolves, and shep then cannot exec it. The release builds cleanly, fails
    // verification, and is rolled back for a reason nothing reports.
    //
    // Set from the handle rather than the path, so it lands on the file that
    // was actually written rather than on whatever the name resolves to now.
    // Same reasoning as the walked open above.
    //
    // Masked to the ordinary nine bits, which is the difference between
    // copying a mode and copying a privilege. `Permissions` carries setuid,
    // setgid and the sticky bit too, and this copy runs in the PARENT at the
    // dog's own uid, which shep's docs recommend be root. So a build dropped to
    // `svc` could write `target/x` with mode 04755, and this would create a
    // root-owned file in the release and make it setuid: anyone who can run it
    // is root, and the release is what the app serves.
    //
    // Introduced by the fix one commit earlier that made the copy keep the
    // executable bit. Preserving the mode was right; preserving all of it was
    // not.
    let mode = opened.permissions().mode() & 0o777;
    sink.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(Error::at(to))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::contain::artifact_roots;
    use super::super::test_support::{TEST_BUILD_BUDGET, build};
    use super::super::{BuildSpec, run};
    use super::*;
    use crate::fixtures;

    /// fails if an artifact the build never produced is reported as copied.
    ///
    /// With no `CARGO_TARGET_DIR` redirect, `copy_artifact`'s source and
    /// destination collapse to the same release-relative path, so the
    /// already-where-it-belongs branch is taken for EVERY declared artifact.
    /// Returning `Ok` there without looking made an artifact that was never
    /// written indistinguishable from one that was: a build exiting 0 without
    /// producing what it declared, or a typo in `artifacts`, rode through the
    /// swap and the reload and surfaced as a runtime failure in a release the
    /// operator had been told was deployed.
    #[tokio::test]
    async fn an_artifact_the_build_did_not_produce_is_a_failure() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            env: BTreeMap::new(),
            artifacts: vec![PathBuf::from("dist/app.js")],
        };

        let err = build(rel.path(), &spec)
            .await
            .expect_err("a declared artifact that is not there must fail the build");

        assert!(
            format!("{err}").contains("did not leave there"),
            "must say what is missing and why: {err}"
        );
    }

    /// fails if a copied artifact keeps setuid, setgid or the sticky bit.
    ///
    /// The copy runs in the PARENT, at the dog's own uid, which shep's docs
    /// recommend be root so it can drop per app. So the destination is created
    /// root-owned. A build dropped to an unprivileged `user` that writes
    /// `target/x` with mode 04755 would then have this hand it back as a
    /// root-owned setuid executable inside the release, which is what the app
    /// serves. Anyone who can run it is root.
    ///
    /// Introduced by the commit that made the copy preserve the executable
    /// bit. Preserving the mode was right; preserving all of it was not.
    #[tokio::test]
    async fn a_copied_artifact_does_not_keep_setuid() {
        use std::os::unix::fs::PermissionsExt;

        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let built = cache.path().join("koji");
        std::fs::write(&built, b"#!/bin/sh\necho hi\n").expect("the build's output");
        std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o4755)).expect("chmod");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/koji")],
        };

        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds and copies");

        let mode = std::fs::metadata(rel.path().join("target/koji"))
            .expect("copied")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o7777,
            0o755,
            "the copy must keep the permission bits and drop the privilege ones"
        );
    }

    /// fails if a copied artifact loses the source's permission bits.
    ///
    /// This is the arrangement the redirect branch exists for: cargo builds
    /// into `CARGO_TARGET_DIR`, and the binary is copied back so a `script`
    /// pointing into `target/release` resolves. `fs::copy` carried the mode.
    /// Replacing it with `OpenOptions` plus `io::copy` did not, so 0755
    /// arrived as 0644, shep could not exec the binary, and a release that
    /// built cleanly failed verification and rolled back with nothing saying
    /// why.
    ///
    /// The old test asserted the destination merely existed, which a
    /// non-executable file does.
    #[tokio::test]
    async fn a_copied_artifact_keeps_the_source_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        let built = cache.path().join("koji");
        std::fs::write(&built, b"#!/bin/sh\necho hi\n").expect("the build's output");
        std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/koji")],
        };

        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds and copies");

        let copied = rel.path().join("target/koji");
        let mode = std::fs::metadata(&copied)
            .expect("copied")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "the copy must be executable, or shep cannot start the release it just built"
        );
    }

    /// fails if an artifact can truncate the file it is copying.
    ///
    /// `link_cache` symlinks `release/target` at the dog's own cache, so a
    /// `CARGO_TARGET_DIR` pointing there gives a source and a destination that
    /// differ as strings and name one file. Opening the destination for
    /// writing then empties the source before anything is read.
    ///
    /// The original `fs::copy` version guarded this lexically and said so in
    /// its own doc. Replacing the copy with an explicit open reopened the
    /// hole, which is why the guard now compares device and inode like the
    /// containment check above it rather than comparing spellings.
    #[tokio::test]
    async fn an_artifact_that_is_already_where_it_belongs_is_not_truncated() {
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        std::fs::create_dir_all(cache.path().join("release")).expect("cache");
        std::fs::write(cache.path().join("release/koji"), b"binary").expect("built");
        // Exactly what `link_cache` does.
        std::os::unix::fs::symlink(cache.path(), rel.path().join("target")).expect("link");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/release/koji")],
        };
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");

        assert_eq!(
            std::fs::read(cache.path().join("release/koji")).expect("still there"),
            b"binary",
            "the artifact must survive being copied onto itself"
        );
    }

    /// fails if an artifact naming one of the two roots is reported as a race.
    ///
    /// `artifact_source` strips `target` and rejoins the rest onto
    /// `CARGO_TARGET_DIR`, so `artifacts = ["target"]` under a redirect
    /// resolves its SOURCE to the redirect itself. Point that at the dog's own
    /// cache, as the arrangement this whole branch exists for does, and the
    /// source IS a root: `Walk` has no last component to open and answers
    /// `None`.
    ///
    /// Refusing is right. Saying "changed underneath the check" is not, because
    /// nothing changed underneath anything, and an operator who reads that goes
    /// looking for a build job that does not exist.
    #[tokio::test]
    async fn an_artifact_naming_a_root_itself_is_refused_as_a_directory() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let roots = artifact_roots(release.path(), cache.path());
        let env: BTreeMap<String, String> = [(
            "CARGO_TARGET_DIR".to_owned(),
            cache.path().display().to_string(),
        )]
        .into();

        let err = copy_artifact(release.path(), &roots, &env, Path::new("target"))
            .expect_err("an artifact naming the cache root must be refused");

        assert!(
            !format!("{err}").contains("changed underneath"),
            "nothing moved, so the refusal must not blame a race: {err}"
        );
        assert!(
            format!("{err}").contains("rather than a file"),
            "must say the entry names a directory: {err}"
        );
    }

    /// fails if a directory declared as an artifact gets as far as creating
    /// things in the release before it is refused.
    ///
    /// `O_RDONLY | O_NOFOLLOW` on a directory succeeds, and the source's type
    /// was never checked, so `artifacts = ["target/dist"]` under a redirect
    /// opened the directory, walked the destination, created its parents and
    /// then the destination itself, and only failed in `io::copy` with
    /// `EISDIR`. The build fails either way. What it left behind was an empty
    /// file in the release and a directory tree made for it.
    ///
    /// Same reasoning as the lexical check at the top of `copy_artifact`: a
    /// refusal that can happen before anything is created should.
    #[tokio::test]
    async fn a_directory_declared_as_an_artifact_creates_nothing() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        std::fs::create_dir(cache.path().join("dist")).expect("the build's output dir");
        let roots = artifact_roots(release.path(), cache.path());
        let env: BTreeMap<String, String> = [(
            "CARGO_TARGET_DIR".to_owned(),
            cache.path().display().to_string(),
        )]
        .into();

        let err = copy_artifact(release.path(), &roots, &env, Path::new("target/dist"))
            .expect_err("a directory is not an artifact");

        assert!(
            format!("{err}").contains("not a regular file"),
            "must say what is wrong with the entry: {err}"
        );
        assert!(
            !release.path().join("target").exists(),
            "a refusal this early must not leave a directory tree in the release"
        );
    }

    /// fails if a named pipe declared as an artifact stops the copy dead.
    ///
    /// Opening a FIFO for reading blocks until something opens it for writing,
    /// and the regular-file check runs on the handle, so it never got the
    /// chance. The build's own budget does not cover this: the child is
    /// reaped and its group killed before any artifact is copied, so a build
    /// that ran `mkfifo target/koji` and exited 0 left the dog waiting for a
    /// writer that was already dead. Forever, and with the tree lock held.
    ///
    /// `O_NONBLOCK` is what makes the type check reachable. It changes
    /// nothing for a regular file, which is the only thing this copies.
    ///
    /// Run on its own thread with a deadline, because the failure being
    /// tested is a hang and an assertion cannot be reached from inside one.
    #[test]
    fn a_named_pipe_declared_as_an_artifact_does_not_stop_the_copy() {
        let release = fixtures::tempdir();
        let cache = fixtures::tempdir();
        let made = std::process::Command::new("mkfifo")
            .arg(cache.path().join("koji"))
            .status()
            .expect("mkfifo");
        assert!(
            made.success(),
            "the fixture needs a named pipe to prove anything"
        );

        let roots = artifact_roots(release.path(), cache.path());
        let env: BTreeMap<String, String> = [(
            "CARGO_TARGET_DIR".to_owned(),
            cache.path().display().to_string(),
        )]
        .into();

        let (tell, heard) = std::sync::mpsc::channel();
        let release_path = release.path().to_owned();
        std::thread::spawn(move || {
            let answer = copy_artifact(&release_path, &roots, &env, Path::new("target/koji"));
            let _ = tell.send(format!("{:?}", answer.err()));
        });

        let answer = heard
            .recv_timeout(Duration::from_secs(10))
            .expect("the copy must answer rather than wait for a writer that may never come");
        assert!(
            answer.contains("not a regular file"),
            "a named pipe is not an artifact: {answer}"
        );
    }
}
