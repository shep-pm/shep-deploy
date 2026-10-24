//! Where an artifact may really land: the two roots, and the check by name
//! that a path resolves under one of them.

use std::path::{Path, PathBuf};

#[cfg(doc)]
use super::contained_artifacts;

/// The deepest ancestor of `path` that exists, fully resolved.
///
/// `canonicalize` fails on a path that does not exist yet, and an artifact's
/// destination usually does not, so this walks up until something does. What
/// comes back has every symlink on it followed, which is the whole point: a
/// destination is only safe if the directory it will really land in is safe.
pub(super) fn resolve_deepest(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_owned();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return Some(real);
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// Whether `candidate` really lands under one of `roots`, symlinks followed.
///
/// The lexical check in [`contained_artifacts`] cannot answer this, and that
/// is not a gap in it but a limit of strings. A repository can commit a
/// symlink, and `crate::shared::link_cache` leaves a `target` entry alone when
/// the release already ships one, so `target/out` with no `..` and no leading
/// slash walks wherever that committed link points. Demonstrated 2026-08-28:
/// a release shipping `target -> /tmp/outside` had `target/victim` written
/// through it, at the dog's uid, which is root under the arrangement shep's
/// own docs recommend.
pub(super) fn lands_within(roots: &[PathBuf], candidate: &Path) -> bool {
    resolve_deepest(candidate).is_some_and(|real| roots.iter().any(|root| real.starts_with(root)))
}

/// The two places an artifact may really land, resolved: the release and
/// the dog's own build cache for this sheep.
///
/// `cache` is passed in rather than read from `release/target`, which is
/// exactly the entry an attacker controls: `link_cache` leaves a release's
/// own `target` alone when it ships one.
///
/// Two escapes measured 2026-08-28, both writing or reading at the dog's uid
/// because the copy runs in the parent, after the build's own drop:
/// `target -> /tmp/outside` wrote through the committed link, and
/// `CARGO_TARGET_DIR = /any/path` with `artifacts = ["target/id_rsa"]` read
/// an arbitrary file into the release, where a static-serving app then
/// hands it out over HTTP.
pub(super) fn artifact_roots(release: &Path, cache: &Path) -> [PathBuf; 2] {
    [
        release
            .canonicalize()
            .unwrap_or_else(|_| release.to_owned()),
        cache.canonicalize().unwrap_or_else(|_| cache.to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TEST_BUILD_BUDGET, build};
    use super::super::{BuildSpec, run};
    use crate::fixtures;
    use std::path::PathBuf;

    /// fails if `CARGO_TARGET_DIR` can read a file from anywhere on the host.
    ///
    /// The simplest of the three escapes and the one needing no trick at all:
    /// `env` is app config, and nothing validated where it
    /// pointed. `artifacts = ["target/id_rsa"]` is relative
    /// and has no `..`, so both lexical guards pass, and `copy_artifact` then
    /// reads whatever that source names into the release. No build code runs.
    ///
    /// It matters because the copy happens in THIS process, at the dog's uid,
    /// which shep's docs recommend running as root. And a release is the
    /// directory a static-serving app hands out over HTTP.
    #[tokio::test]
    async fn a_source_outside_the_release_and_cache_is_refused() {
        let rel = fixtures::fixture_release(&[]);
        let elsewhere = fixtures::tempdir();
        std::fs::write(elsewhere.path().join("id_rsa"), b"PRIVATE KEY").expect("secret");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                elsewhere.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/id_rsa")],
        };
        let err = build(rel.path(), &spec)
            .await
            .expect_err("a source outside the tree must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert!(
            !rel.path().join("target/id_rsa").exists(),
            "nothing may be read into the release"
        );
    }

    /// fails if a committed symlink can carry a write out of the release.
    ///
    /// No `..` and no absolute path, so the lexical guard passes and cannot
    /// help: the escape is a filesystem object, not a string. A repository can
    /// commit a symlink, and `crate::shared::link_cache` leaves a `target`
    /// entry alone when the release already ships one, so the dog writes
    /// straight through it at its own uid.
    #[tokio::test]
    async fn a_committed_target_symlink_cannot_carry_a_write_out() {
        let rel = fixtures::fixture_release(&[]);
        let outside = fixtures::tempdir();
        std::fs::write(outside.path().join("victim"), b"original").expect("victim");
        std::os::unix::fs::symlink(outside.path(), rel.path().join("target")).expect("link");

        let cache = fixtures::tempdir();
        std::fs::write(cache.path().join("victim"), b"ATTACKER").expect("payload");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/victim")],
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect_err("a write through a committed symlink must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("victim")).expect("still there"),
            "original",
            "the file outside the release must be untouched"
        );
    }

    /// fails if a symlinked component BELOW `target` can carry a write out.
    ///
    /// `a_committed_target_symlink_cannot_carry_a_write_out` covers `target`
    /// itself. This covers a component one level deeper, which is where the
    /// round-7 review expected the guard to end, because `O_NOFOLLOW` is
    /// honoured on the leaf only. Two things hold the line instead, and the
    /// depth of the component does not matter to either: `lands_within`
    /// resolves the deepest existing ancestor before anything is created, and
    /// `Walk` opens every directory below a root against the descriptor of
    /// the one above it.
    ///
    /// Verified 2026-08-28 that the equivalent link pointing INSIDE the
    /// release does copy through, and that this is correct rather than a
    /// second hole: `shared::link_cache` makes `release/target` a link at the
    /// dog's own cache, so refusing every symlinked component would refuse
    /// the ordinary cargo arrangement.
    #[tokio::test]
    async fn a_symlinked_component_below_target_cannot_carry_a_write_out() {
        let rel = fixtures::fixture_release(&[]);
        let cache = fixtures::tempdir();
        std::fs::create_dir(cache.path().join("sub")).expect("build output dir");
        std::fs::write(cache.path().join("sub/out"), b"ARTIFACT").expect("build output");

        let outside = fixtures::tempdir();
        std::fs::create_dir(rel.path().join("target")).expect("target");
        std::os::unix::fs::symlink(outside.path(), rel.path().join("target/sub")).expect("link");

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/sub/out")],
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            None,
            &[],
            cache.path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect_err("a write through a symlinked component must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "must say why: {err}"
        );
        assert!(
            !outside.path().join("out").exists(),
            "nothing may be written outside the release"
        );
    }

    /// fails if a `..` in a declared artifact can reach outside the release.
    ///
    /// This is a security boundary, not tidiness. `copy_artifact` runs in
    /// THIS process after the child exited, so the `uid`/`gid` set on the
    /// build's `Command` never applied to it; under the arrangement shep's
    /// docs recommend that is root. Measured 2026-08-28 against the unfixed
    /// code: this exact spec overwrote the tree's own `deploy.toml`, whose
    /// `remote` every later fetch reads, so a commit on the tracked branch
    /// could repoint the deploy at a repository of its own.
    ///
    /// `CARGO_TARGET_DIR` is load-bearing here and the test is worthless
    /// without it. With no override, `from` and `to` are the same expression,
    /// so the `..` collapses to `from == to` and the self-copy guard returns
    /// `Ok` before reaching anything this test is about. The unfixed code
    /// passes a version of this test that omits the override.
    #[tokio::test]
    async fn an_artifact_that_escapes_the_release_is_refused() {
        let tree = fixtures::tempdir();
        let release = tree.path().join("releases/abc123");
        std::fs::create_dir_all(&release).unwrap();
        let sentinel = tree.path().join("deploy.toml");
        std::fs::write(&sentinel, b"remote = \"https://real.example/repo.git\"").unwrap();

        let cache = fixtures::tempdir();
        let stolen = cache.path().join("deploy.toml");
        std::fs::write(&stolen, b"remote = \"https://attacker.example/evil.git\"").unwrap();

        let spec = BuildSpec {
            command: Some("true".into()),
            env: [(
                "CARGO_TARGET_DIR".into(),
                cache.path().join("a/b/c").display().to_string(),
            )]
            .into(),
            artifacts: vec![PathBuf::from("target/../../../deploy.toml")],
        };

        let err = build(&release, &spec)
            .await
            .expect_err("an escaping artifact must be refused");
        assert!(
            format!("{err}").contains("outside the release"),
            "the refusal must say why, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "remote = \"https://real.example/repo.git\"",
            "the tree's own state file must be untouched"
        );
    }
}
