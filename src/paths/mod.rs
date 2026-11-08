//! The on-disk layout under `$SHEP_HOME/deploy`.
//!
//! [`Tree`] is the one place that knows this layout. Every other module asks
//! it for a path rather than joining strings itself, so the layout can only
//! drift in one place if it ever needs to.
//!
//! ```text
//! $SHEP_HOME/deploy/<sheep>/
//! ├── git                  the bare clone; object store shared by every
//! │                        worktree release. A link to the shared clone
//! │                        below, or, on a tree set up before those
//! │                        existed, a directory of its own
//! ├── current              a link into the shared releases below, swapped
//! │                        with rename(2) at cutover. The sheep's `cwd` is
//! │                        this path, permanently
//! ├── deploy.lock          exclusive flock so only one deploy runs at once
//! ├── deploy.toml          the sheep's `State` - see `crate::state`
//! ├── releases/            where releases lived when every sheep kept its
//! │                        own. Nothing writes here any more - see
//! │                        [`Tree::superseded_releases`]
//! ├── complete/            completion markers of the same vintage - see
//! │                        [`Tree::superseded_completions`]
//! └── cache/               a build cache left behind by the version that kept
//!                          one per sheep - see [`Tree::superseded_cache`]
//!
//! $SHEP_HOME/deploy/repos/<key>/
//! ├── git/                 the shared bare clone, one per remote; see [`Repo`]
//! ├── git.lock             held while one fetch into it runs
//! ├── git.build.lock       held while one release from this repository builds
//! ├── releases/<sha>-<build>/
//! │                        a git worktree per built release, shared by every
//! │                        sheep of this remote that builds that sha with that
//! │                        command, as that user and with that build
//! │                        environment. `<build>` is [`BuildKey`]'s hash - see
//! │                        it for what a release is keyed by
//! ├── complete/<sha>-<build>/
//! │                        empty, and named exactly what its release is named.
//! │                        Its presence means that worktree was checked out
//! │                        AND built. Outside the release on purpose - see
//! │                        [`Tree::completion`]
//! └── cache/target/        the dog's build cache, one per remote, symlinked
//!                          into every release of every sheep that deploys
//!                          that remote as `target`, so builds stay warm and a
//!                          hardcoded `./target/release/x` still resolves. It
//!                          outlives the clone whenever a tree that owns its own
//!                          clone still deploys that remote - see
//!                          `crate::retention::release_clone`
//! ```

mod build_key;
mod repo;
mod tree;

pub use build_key::{BuildKey, is_release_name, release_sha};
pub use repo::{Repo, repo_at};
pub use tree::Tree;

use std::path::{Component, Path, PathBuf};

use crate::error::Error;

/// The sheep name that [`Repo`] directories live under, so no sheep can be
/// called it. `setup` refuses it.
pub const REPOS: &str = "repos";

/// 64-bit FNV-1a, the hash [`Repo`]'s key and [`BuildKey`] both carry.
///
/// Specified, so a key this crate wrote last release is one it can still
/// read: `std`'s hashers promise nothing across builds.
pub(super) fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// What is under `<shep_home>/deploy`: every sheep that is a deploy target,
/// by name, and every directory there that holds a record and cannot be
/// named.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Targets {
    /// The targets, sorted by name.
    pub named: Vec<String>,
    /// Directories holding a `deploy.toml` whose name cannot be a sheep's:
    /// not valid UTF-8, or carrying a character that would rewrite a log
    /// line (see `shared::forges_a_line`). Each is a target
    /// the dog cannot poll or restore, reported by every caller as its own
    /// row rather than skipped, and rather than stopping the listing for the
    /// targets that can be named.
    pub unnamed: Vec<PathBuf>,
}

/// Every sheep that is a deploy target, by name, and every directory that
/// holds a record and cannot be named.
///
/// A target is a directory under `<shep_home>/deploy` holding a
/// `deploy.toml`. Reading the directory rather than a list held anywhere
/// else is what makes a tree self-describing: it survives the dog being
/// rehomed, re-adopted under a different name, or replaced by a different
/// deploy dog entirely, which is the same reasoning that put per-target
/// state in the tree instead of in `[dog.<name>]`.
///
/// # Errors
/// [`Error::Io`], naming `<shep_home>/deploy`, if it exists but cannot be
/// listed. An absent directory is an empty list, not an error: that is
/// every shepherd with no targets yet. A directory that cannot be named is
/// not an error either, it is in [`Targets::unnamed`]: one such entry used
/// to stop the whole listing and with it every deploy of every target.
pub fn targets(shep_home: &Path) -> Result<Targets, Error> {
    let root = shep_home.join("deploy");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Targets::default());
        }
        Err(source) => return Err(Error::Io { path: root, source }),
    };

    let mut found = Targets::default();
    for entry in entries {
        let entry = entry.map_err(Error::at(&root))?;
        if !entry.path().join("deploy.toml").is_file() {
            continue;
        }
        match entry.file_name().to_str() {
            Some(name) if is_sheep_name(name) => found.named.push(name.to_owned()),
            _ => found.unnamed.push(entry.path()),
        }
    }
    found.named.sort();
    found.unnamed.sort();
    Ok(found)
}

/// Whether `sheep` names one sheep, rather than a path.
///
/// [`Tree::for_sheep`] joins this onto `$SHEP_HOME/deploy`, and `PathBuf::join`
/// REPLACES the whole path when what it is given is absolute. So a sheep name
/// of `/tmp/anywhere` does not traverse out of the tree, it discards the tree
/// entirely and roots itself wherever it says. `..` traverses out the ordinary
/// way. Either one puts a deploy tree, and everything that later prunes and
/// removes inside it, somewhere the operator did not point it.
///
/// Checked at the one place a name arrives from outside, which is the command
/// line. The poll loop's names come from directory entries under
/// `$SHEP_HOME/deploy` that this crate created, so they are already inside.
///
/// The test is that the string is exactly one ordinary path component with
/// nothing in it that would rewrite a log line (`shared::forges_a_line`'s
/// set): that rejects the empty string, `.`, `..`, anything absolute,
/// anything with a separator in it and anything invisible or bidirectional,
/// without a charset rule that would have to guess at what an operator may
/// call their app.
#[must_use]
pub fn is_sheep_name(sheep: &str) -> bool {
    let mut components = Path::new(sheep).components();
    let one_ordinary_component =
        matches!(components.next(), Some(Component::Normal(only)) if only == sheep);

    // Nothing that forges a line either. The name is interpolated into every
    // log line about the target, and a newline or a bidi override in it
    // rewrites the line around it; the set is `shared::forges_a_line`'s, the
    // same one messages replace.
    one_ordinary_component
        && components.next().is_none()
        && !sheep.chars().any(crate::shared::forges_a_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `is_sheep_name` accepts a name that would traverse out of
    /// the tree, or rejects an ordinary one. This is the one check standing
    /// between an operator-typed name and a tree rooted somewhere nobody
    /// pointed it.
    #[test]
    fn is_sheep_name_accepts_one_component_and_rejects_everything_else() {
        for accepted in ["bpm", "reactmap-staging", "web.2"] {
            assert!(is_sheep_name(accepted), "should accept {accepted:?}");
        }
        for rejected in ["", ".", "..", "/tmp/x", "../x", "a/b", "a/"] {
            assert!(!is_sheep_name(rejected), "should reject {rejected:?}");
        }
    }

    /// fails if a directory whose name is not valid UTF-8 is silently
    /// dropped from the target list. A target the dog cannot name never
    /// polls and never restores, with nothing said anywhere, so this has
    /// to be a loud error instead of a skip.
    #[test]
    fn a_non_utf8_target_name_is_an_error_naming_the_entry() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let home = tempfile::tempdir().expect("tempdir");
        let deploy = home.path().join("deploy");
        let bad_dir = deploy.join(OsStr::from_bytes(&[0xff, 0xfe]));
        if let Err(err) = std::fs::create_dir_all(&bad_dir) {
            // APFS refuses a name that is not valid UTF-8 outright, so on
            // macOS the entry this pins cannot exist. Linux filesystems
            // accept it, and CI runs there.
            eprintln!("skipped: this filesystem refuses the name ({err})");
            return;
        }
        std::fs::write(bad_dir.join("deploy.toml"), "").expect("write deploy.toml");

        let found = targets(home.path()).expect("the listing itself succeeds");
        assert_eq!(
            found.unnamed,
            vec![bad_dir],
            "the entry is reported, not skipped"
        );
        assert!(found.named.is_empty());
    }

    /// fails if a target the dog cannot name stops the targets it can. One
    /// such entry used to abort the whole listing, and `poll::tick` then
    /// deployed nothing, every tick, for as long as it sat there.
    #[test]
    fn an_unnameable_target_does_not_hide_the_named_ones() {
        let home = tempfile::tempdir().expect("tempdir");
        let deploy = home.path().join("deploy");
        for name in ["web", "bad\nname"] {
            let dir = deploy.join(name);
            std::fs::create_dir_all(&dir).expect("dir");
            std::fs::write(dir.join("deploy.toml"), "").expect("record");
        }

        let found = targets(home.path()).expect("lists");

        assert_eq!(found.named, vec!["web".to_owned()]);
        assert_eq!(found.unnamed, vec![deploy.join("bad\nname")]);
    }
}
