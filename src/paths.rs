//! The on-disk layout under `$SHEP_HOME/deploy/<sheep>/`.
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
//! ├── releases/<sha>/      a git worktree per built release
//! ├── cache/target/        the dog's build cache, symlinked into every
//! │                        release as `target` so builds stay warm and a
//! │                        hardcoded `./target/release/x` still resolves
//! ├── current -> releases/<sha>   swapped with rename(2) at cutover
//! ├── complete/<sha>       per-release completion marker, kept outside the
//! │                        release on purpose - see `Tree::completion`
//! ├── deploy.lock          exclusive flock so only one deploy runs at once
//! └── deploy.toml          the sheep's `State` - see `crate::state`
//!
//! $SHEP_HOME/deploy/repos/<key>/
//! ├── git/                 the shared bare clone, one per remote; see [`Repo`]
//! └── git.lock             held while one fetch into it runs
//! ```

use std::path::{Component, Path, PathBuf};

use crate::error::Error;

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

/// The sheep name that [`Repo`] directories live under, so no sheep can be
/// called it. `setup` refuses it.
pub const REPOS: &str = "repos";

/// A bare clone shared by every tree whose remote is the same repository,
/// at `$SHEP_HOME/deploy/repos/<key>/git`.
///
/// The key is `<tail>-<hash>`: `tail` is the last path component of the
/// normalized remote, reduced to `[A-Za-z0-9._-]` for whoever reads the
/// directory, and `hash` is the 64-bit FNV-1a of the whole normalized remote
/// in hex, which is what tells two repositories apart. FNV-1a is
/// specified, so the key does not change between builds or toolchains, which
/// `std`'s hashers do not promise. The key is always one ordinary path
/// component, whatever the remote says.
///
/// The remote is normalized before hashing: surrounding whitespace and
/// trailing slashes go, then one trailing `.git`, and the scheme and host of
/// a `scheme://host/path` remote are lowercased. The path is not, since a
/// host serves `Shep-pm/x` and `shep-pm/x` as different repositories. Remotes
/// in other spellings (`git@host:path` against `https://host/path`) are
/// different keys, and so two clones: unifying them is a guess about the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The shared clone for `remote`.
    #[must_use]
    pub fn for_remote(shep_home: &Path, remote: &str) -> Self {
        let normalized = normalized_remote(remote);
        let tail: String = normalized
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
        let tail = tail.trim_start_matches('.');
        let tail = if tail.is_empty() { "repo" } else { tail };
        Self {
            root: shep_home
                .join("deploy")
                .join(REPOS)
                .join(format!("{tail}-{:016x}", fnv1a(&normalized))),
        }
    }

    /// The clone's own directory: everything of it is inside this.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bare clone itself.
    #[must_use]
    pub fn git(&self) -> PathBuf {
        self.root.join("git")
    }

    /// The directory `$SHEP_HOME` keeps every shared clone in.
    #[must_use]
    pub fn all(shep_home: &Path) -> PathBuf {
        shep_home.join("deploy").join(REPOS)
    }
}

/// `remote` as [`Repo`] keys it. See there for what is folded together.
fn normalized_remote(remote: &str) -> String {
    let trimmed = remote.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix(".git")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let authority_end = trimmed.find("://").map_or(0, |scheme| {
        let after = scheme + "://".len();
        trimmed[after..].find('/').map_or(trimmed.len(), |at| after + at)
    });
    format!(
        "{}{}",
        trimmed[..authority_end].to_ascii_lowercase(),
        &trimmed[authority_end..]
    )
}

/// 64-bit FNV-1a, the hash [`Repo`]'s key carries.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The deploy tree for one sheep.
///
/// Holds only the tree's root; every other path is derived from it on
/// demand rather than stored, so there is exactly one fact to keep
/// consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    root: PathBuf,
    sheep: String,
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

impl Tree {
    /// The tree for `sheep`, rooted at `<shep_home>/deploy/<sheep>`.
    ///
    /// Sheep name, not repository, is the key: the same repository can be
    /// deployed under several sheep names (Rin runs `bpm`, `ctm` and `opm`
    /// off one `ReactMap` checkout), and each gets its own tree.
    #[must_use]
    pub fn for_sheep(shep_home: &Path, sheep: &str) -> Self {
        // The real check lives in `main::route`; this keeps the contract
        // next to the constructor instead of trusting every caller to have
        // gone through there first.
        debug_assert!(
            is_sheep_name(sheep),
            "a sheep name is one path component, got {sheep:?}"
        );
        Self {
            root: shep_home.join("deploy").join(sheep),
            sheep: sheep.to_owned(),
        }
    }

    /// The sheep this tree belongs to.
    ///
    /// Kept as its own field rather than read back out of the last
    /// component of `root`: the deploy sequence passes this name to the
    /// shepherd, and a name that has been through a `Path` round trip is a
    /// name that can come back lossy.
    #[must_use]
    pub fn sheep(&self) -> &str {
        &self.sheep
    }

    /// The tree's own directory: everything below is inside this.
    ///
    /// Named because an abandoned cutover has to tell an operator which
    /// directory to remove before trying again, and `releases()` is not it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bare clone and its object store, shared by every release's
    /// worktree.
    #[must_use]
    pub fn git(&self) -> PathBuf {
        self.root.join("git")
    }

    /// Whether this tree's `git` is a clone of its own, as every tree's was
    /// before clones were shared, rather than a link to a shared one.
    #[must_use]
    pub fn owns_clone(&self) -> bool {
        std::fs::symlink_metadata(self.git()).is_ok_and(|meta| meta.file_type().is_dir())
    }

    /// The shared clone this tree's [`Self::git`] links to, or `None` for a
    /// tree that owns its clone or has none.
    #[must_use]
    pub fn linked_clone(&self) -> Option<Repo> {
        let target = std::fs::read_link(self.git()).ok()?;
        let root = target.parent()?;
        (target.file_name()? == "git" && root.parent()?.file_name()? == REPOS).then(|| Repo {
            root: root.to_owned(),
        })
    }

    /// Points this tree's [`Self::git`] at `repo`'s clone, replacing a link to
    /// another one. Does nothing to a link already there.
    ///
    /// Never replaces a clone the tree owns: callers check [`Self::owns_clone`]
    /// first, and a directory here is an error.
    ///
    /// # Errors
    /// [`Error::Io`], naming the link, if it cannot be read, replaced or
    /// made, or if it is a directory.
    pub fn link_clone(&self, repo: &Repo) -> Result<(), Error> {
        let (link, target) = (self.git(), repo.git());
        match std::fs::read_link(&link) {
            Ok(current) if current == target => return Ok(()),
            Ok(_) => std::fs::remove_file(&link).map_err(Error::at(&link))?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path: link, source }),
        }
        std::os::unix::fs::symlink(target, &link).map_err(Error::at(link))
    }

    /// The directory holding every built release, one per sha.
    #[must_use]
    pub fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// One built release's worktree, by full sha.
    #[must_use]
    pub fn release(&self, sha: &str) -> PathBuf {
        self.releases().join(sha)
    }

    /// The symlink a cutover `rename(2)`s onto the new release. The sheep's
    /// `cwd` is this path, permanently, so swapping a release never leaves
    /// a moment where it points at nothing.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Where a release's completion marker lives, keyed by sha.
    ///
    /// OUTSIDE the release, and that is the whole point of the path. The
    /// marker says "this checkout finished", and a marker inside the release
    /// is a file the DEPLOYED REPOSITORY can commit: `git worktree add` then
    /// writes it out as part of the checkout it is supposed to vouch for, so
    /// a kill partway leaves a partial release carrying its own certificate.
    /// That is the round-9 blocker again with the adversary holding the pen.
    ///
    /// Under the tree's own root, which only this dog writes.
    #[must_use]
    pub fn completion(&self, sha: &str) -> PathBuf {
        self.completions().join(sha)
    }

    /// The directory every completion marker lives in.
    ///
    /// Named on its own because retention sweeps it. Deriving it from
    /// `completion("")` gave a path with a trailing separator whose
    /// `parent()` was the tree root, and a sweep of the root removed
    /// `current`. Measured by the suite on 2026-09-03, before it shipped.
    #[must_use]
    pub fn completions(&self) -> PathBuf {
        self.root.join("complete")
    }

    /// The file a deploy takes an exclusive `flock` on, so only one process
    /// at a time works on this tree.
    ///
    /// Its own file rather than the record or the root: `flock` is held for
    /// the life of an open handle, and taking it on `deploy.toml` would tie
    /// the lock's lifetime to a file the deploy rewrites underneath itself.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("deploy.lock")
    }

    /// Where this sheep's [`crate::state::State`] lives.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.root.join("deploy.toml")
    }

    /// The lock every read-modify-write of [`Self::state_file`] holds, so
    /// two writers cannot each rename their own snapshot over the other's.
    ///
    /// Separate from [`Self::lock_file`], which a deploy holds for as long
    /// as a build takes: this one is held for one read and one write, so
    /// `--watch manual` typed during a build lands at once. See
    /// `crate::lock::hold_record`.
    #[must_use]
    pub fn record_lock(&self) -> PathBuf {
        self.root.join("deploy.toml.lock")
    }

    /// The dog's own build cache for this sheep, shared by every release.
    ///
    /// Outside `releases/` deliberately: retention removes worktrees under
    /// there, and a cache swept up with one would turn the next deploy into
    /// a from-scratch build.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory every release's `target` symlink points at.
    ///
    /// Named `target` rather than pointed at by `CARGO_TARGET_DIR` because
    /// setting that variable means `./target` is never created, and a build
    /// command ending in `cp ./target/release/koji koji` then exits 1.
    /// Measured, and the reason this is a symlink at all.
    #[must_use]
    pub fn cache_target(&self) -> PathBuf {
        self.cache().join("target")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `Tree` builds a path by joining the wrong segments, or in
    /// the wrong order. `Tree` is the one place allowed to know this
    /// layout, so a mistake here is a mistake everywhere.
    #[test]
    fn every_path_matches_the_documented_layout() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "bpm");
        assert_eq!(tree.git(), Path::new("/srv/shep/deploy/bpm/git"));
        assert_eq!(tree.releases(), Path::new("/srv/shep/deploy/bpm/releases"));
        assert_eq!(
            tree.release("a1b2c3d"),
            Path::new("/srv/shep/deploy/bpm/releases/a1b2c3d")
        );
        assert_eq!(tree.current(), Path::new("/srv/shep/deploy/bpm/current"));
        assert_eq!(
            tree.completion("a1b2c3d"),
            Path::new("/srv/shep/deploy/bpm/complete/a1b2c3d")
        );
        assert_eq!(
            tree.lock_file(),
            Path::new("/srv/shep/deploy/bpm/deploy.lock")
        );
        assert_eq!(
            tree.state_file(),
            Path::new("/srv/shep/deploy/bpm/deploy.toml")
        );
    }

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

    /// fails if two spellings of one remote get different clones, or two
    /// remotes get one. The clone is keyed by the remote alone, so a key
    /// that splits `.git` from no `.git` stores a repository twice, and one
    /// that merges two repositories deploys one's commits as the other's.
    #[test]
    fn a_repo_is_keyed_by_its_normalized_remote() {
        let home = Path::new("/srv/shep");
        let key = |remote: &str| Repo::for_remote(home, remote).root().to_owned();

        let one = key("https://github.com/shep-pm/shep-deploy");
        for same in [
            "https://github.com/shep-pm/shep-deploy.git",
            "https://github.com/shep-pm/shep-deploy/",
            "https://github.com/shep-pm/shep-deploy.git/",
            "  https://github.com/shep-pm/shep-deploy\n",
            "HTTPS://GitHub.com/shep-pm/shep-deploy",
        ] {
            assert_eq!(key(same), one, "{same:?} is the same remote");
        }
        for other in [
            "https://github.com/shep-pm/shep",
            "https://github.com/Shep-pm/shep-deploy",
            "git@github.com:shep-pm/shep-deploy",
            "/srv/git/shep-deploy",
        ] {
            assert_ne!(key(other), one, "{other:?} is a different remote");
        }
    }

    /// fails if the shared clone leaves `$SHEP_HOME/deploy/repos/<key>/git`,
    /// or its key is more than one path component. A key with a separator
    /// in it would put the clone, and the removal that follows it, somewhere
    /// the remote's text chose.
    #[test]
    fn the_shared_clone_lives_under_repos_in_one_component() {
        let repo = Repo::for_remote(Path::new("/srv/shep"), "../../etc/passwd");
        let rel = repo
            .git()
            .strip_prefix("/srv/shep/deploy/repos")
            .expect("under repos")
            .to_owned();
        let parts: Vec<_> = rel.components().collect();
        assert_eq!(parts.len(), 2, "<key>/git, got {rel:?}");
        assert_eq!(parts[1].as_os_str(), "git");
        assert!(
            matches!(parts[0], Component::Normal(_)),
            "key is an ordinary component: {rel:?}"
        );
    }

    /// fails if a tree stops knowing which sheep it belongs to. The
    /// deploy sequence sends this name to the shepherd, so a tree that
    /// answered with the wrong one would reload somebody else's app.
    #[test]
    fn a_tree_knows_its_own_sheep() {
        assert_eq!(
            Tree::for_sheep(Path::new("/srv/shep"), "bpm").sheep(),
            "bpm"
        );
    }

    /// fails if two sheep names collide onto the same tree. The layout is
    /// keyed by sheep name specifically so one repository can be deployed
    /// under several names, each with its own state.
    #[test]
    fn different_sheep_get_different_trees() {
        let a = Tree::for_sheep(Path::new("/srv/shep"), "bpm");
        let b = Tree::for_sheep(Path::new("/srv/shep"), "ctm");
        assert_ne!(a.state_file(), b.state_file());
    }

    /// fails if the cache moves out of the tree or changes name. Every
    /// release symlinks `target` at this one path, so a release built
    /// against a cache at one location and a later release linking another
    /// would silently lose every incremental artifact between them, which
    /// reads as "the build is just slow today".
    #[test]
    fn the_build_cache_lives_in_the_tree() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert_eq!(tree.cache(), Path::new("/srv/shep/deploy/koji/cache"));
        assert_eq!(
            tree.cache_target(),
            Path::new("/srv/shep/deploy/koji/cache/target")
        );
    }

    /// fails if the cache is ever placed inside `releases/`. It has to
    /// outlive every release it serves: retention removes worktrees under
    /// `releases/`, and a cache swept up with one would make the next
    /// deploy a from-scratch build, which for Koji is the exact outcome
    /// this whole mechanism exists to avoid.
    #[test]
    fn the_cache_is_not_inside_releases() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        assert!(!tree.cache().starts_with(tree.releases()));
    }
}
