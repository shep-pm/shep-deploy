//! One remote's shared directory: its clone, its build cache, its
//! releases and their completion markers. See [`Repo`].

use std::path::{Path, PathBuf};

use super::build_key::BuildKey;
use super::{REPOS, fnv1a};

/// One remote's shared directory, `$SHEP_HOME/deploy/repos/<key>`: the bare
/// clone every tree with that remote links its `git` to, and the build cache
/// they build against.
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
        Self::keyed(&shep_home.join("deploy").join(REPOS), remote)
    }

    /// The repository whose directory is `root`, for a caller that came at it
    /// from a link rather than by keying a remote.
    ///
    /// [`crate::paths::Tree::linked_clone`] is the only caller, and it is the one that
    /// checked `root` is a `<deploy>/repos/<key>` of this deploy: whoever lets
    /// go of a clone removes this directory, so that check is what keeps a
    /// hand-written link to somewhere else from having somewhere else removed.
    pub(super) fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }
    /// The clone for `remote` among those under `repos`.
    pub(super) fn keyed(repos: &Path, remote: &str) -> Self {
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
            root: repos.join(format!("{tail}-{:016x}", fnv1a(&normalized))),
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

    /// The build cache every tree that deploys this remote shares.
    ///
    /// Beside the clone the same remote keys, rather than inside any one
    /// tree, so two sheep off one repository compile against one incremental
    /// cache instead of building the same tree once per sheep.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory every release of this remote links in as `target`.
    ///
    /// Named `target` rather than pointed at by `CARGO_TARGET_DIR` because
    /// setting that variable means `./target` is never created, and a build
    /// command ending in `cp ./target/release/koji koji` then exits 1.
    /// Measured, and the reason this is a symlink at all.
    #[must_use]
    pub fn cache_target(&self) -> PathBuf {
        self.cache().join("target")
    }

    /// The directory holding every release built from this remote.
    ///
    /// Per remote and not per sheep, because a build belongs to the repository:
    /// `bpm`, `ctm` and `opm` off one checkout are one tree of worktrees, and
    /// retention of them is one question asked of one directory. See
    /// `crate::retention::prune`, which spares what any of those sheep's
    /// `current` or record names.
    #[must_use]
    pub fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// The directory holding every completion marker for this remote's releases.
    ///
    /// Named on its own because retention sweeps it. Deriving it from a marker
    /// path is the trap: a name built by joining an empty sha leaves a trailing
    /// separator, and its `parent()` is the directory above the one wanted.
    #[must_use]
    pub fn completions(&self) -> PathBuf {
        self.root.join("complete")
    }

    /// One built release: `<releases>/<sha>-<build key>`.
    #[must_use]
    pub fn release(&self, sha: &str, build: &BuildKey) -> PathBuf {
        self.releases().join(format!("{sha}-{}", build.as_str()))
    }

    /// The marker that says `<Self::release>` was checked out and built, named
    /// exactly what that release is named.
    #[must_use]
    pub fn completion(&self, sha: &str, build: &BuildKey) -> PathBuf {
        self.completions().join(format!("{sha}-{}", build.as_str()))
    }
}

/// The repository `remote` shares between the trees beside `deploy`.
///
/// For a caller that has a `deploy` directory rather than a [`Tree`](super::Tree): retention
/// asks what somebody else's tree deploys from, and the answer is keyed by that
/// directory exactly as the tree's own would be.
#[must_use]
pub fn repo_at(deploy: &Path, remote: &str) -> Repo {
    Repo::keyed(&deploy.join(REPOS), remote)
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
        trimmed[after..]
            .find('/')
            .map_or(trimmed.len(), |at| after + at)
    });
    format!(
        "{}{}",
        trimmed[..authority_end].to_ascii_lowercase(),
        &trimmed[authority_end..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Component;

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
}
