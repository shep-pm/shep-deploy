//! One sheep's deploy tree: its clone link, its `current`, its record
//! and its locks. See [`Tree`].

use std::path::{Component, Path, PathBuf};

use crate::error::Error;

use super::build_key::BuildKey;
use super::repo::Repo;
use super::{REPOS, is_sheep_name};

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
    /// directory to remove before trying again, and the `releases/` directory of
    /// [`Self::repo`] is not it.
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
    ///
    /// Only a link to `<this tree's deploy dir>/repos/<key>/git` counts. A
    /// link anywhere else is not a shared clone, whatever it is called, since
    /// whoever lets go of a clone removes its directory.
    #[must_use]
    pub fn linked_clone(&self) -> Option<Repo> {
        let target = std::fs::read_link(self.git()).ok()?;
        let root = target.parent()?;
        let repos = self.root.parent()?.join(REPOS);
        (target.file_name()? == "git"
            && root.parent()? == repos
            && matches!(
                Path::new(root.file_name()?).components().next(),
                Some(Component::Normal(_))
            ))
        .then(|| Repo::at(root))
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

    /// Whether this tree's clone is one `remote` may be fetched into.
    ///
    /// A tree linked to a shared clone is served only by the clone keyed by
    /// `remote`: fetching another remote into it would replace the branches
    /// every other tree linked to it deploys from. A tree that owns its clone
    /// has nothing shared to spoil.
    #[must_use]
    pub fn clone_serves(&self, remote: &str) -> bool {
        self.linked_clone()
            .is_none_or(|linked| linked == self.repo(remote))
    }

    /// The `deploy` directory this tree lives in, which is where the clones
    /// and build caches shared between trees live, and where every tree that
    /// could name one of them lives beside them.
    pub(crate) fn deploy_dir(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
    }

    /// What `remote` shares between the trees beside this one: its clone and
    /// its build cache.
    ///
    /// Keyed from the tree's own location rather than from `$SHEP_HOME`, so a
    /// tree, the clone it links to and the cache it builds against are keyed
    /// by one directory however the home is spelled. [`Self::for_sheep`] roots
    /// every tree under a `deploy` directory, so a tree always has a parent;
    /// one handed a root with none is keyed by that root.
    #[must_use]
    pub fn repo(&self, remote: &str) -> Repo {
        Repo::keyed(&self.deploy_dir().join(REPOS), remote)
    }

    /// One built release's worktree: this tree's repository, `sha`, built by
    /// `build`.
    ///
    /// The build key is part of the answer because a release is not only a
    /// checkout. A sheep whose `command` or `build_env` or `user` changed gets a
    /// release of its own, and one whose three match another sheep's gets that
    /// sheep's release, already built.
    #[must_use]
    pub fn release(&self, remote: &str, sha: &str, build: &BuildKey) -> PathBuf {
        self.repo(remote).release(sha, build)
    }

    /// The symlink a cutover `rename(2)`s onto the new release. The sheep's
    /// `cwd` is this path, permanently, so swapping a release never leaves
    /// a moment where it points at nothing.
    #[must_use]
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }

    /// Where a release's completion marker lives.
    ///
    /// OUTSIDE the release, and that is the whole point of the path. The
    /// marker says "this release was checked out and built", and a marker
    /// inside the release is a file the DEPLOYED REPOSITORY can commit: `git
    /// worktree add` then writes it out as part of the checkout it is supposed
    /// to vouch for, so a kill partway leaves a partial release carrying its
    /// own certificate.
    ///
    /// Beside the release rather than under the tree's root, because the
    /// release is shared: the sheep that built it is not necessarily the one
    /// that asks whether it is finished.
    #[must_use]
    pub fn completion(&self, remote: &str, sha: &str, build: &BuildKey) -> PathBuf {
        self.repo(remote).completion(sha, build)
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

    /// Shorthand for [`Repo::cache_target`] for the repository `remote`
    /// names: the directory every release of that remote links in as
    /// `target`, shared by every sheep that deploys it.
    #[must_use]
    pub fn cache_target(&self, remote: &str) -> PathBuf {
        self.repo(remote).cache_target()
    }

    /// Where this tree's build cache lived before it moved to [`Repo::cache`].
    ///
    /// Nothing writes here. It is named for one question, asked at one place:
    /// whether a release's `target` link is the one an earlier version of this
    /// dog made here, which [`crate::shared::link_cache`] then repoints at
    /// [`Repo::cache_target`]. A release checked out before the move keeps its
    /// old link, and a build writing through it lands outside both roots the
    /// artifacts are confined to, so the link has to follow the cache.
    ///
    /// Nothing removes the directory. A live release's `target` may still name
    /// what is in it, and the app is serving from that release.
    #[must_use]
    pub fn superseded_cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Where this tree's releases lived before they moved to [`Repo::releases`].
    ///
    /// Nothing writes here. It is named for one question, asked at one place:
    /// a release checked out here is a worktree of the clone this tree owned,
    /// and no clone under `repos/` can prune its registration.
    /// `crate::optin::clone` removes the directory when it moves a tree onto a
    /// shared clone, which is the only thing that ever does.
    #[must_use]
    pub fn superseded_releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    /// Where this tree's completion markers lived before they moved to
    /// [`Repo::completions`].
    #[must_use]
    pub fn superseded_completions(&self) -> PathBuf {
        self.root.join("complete")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    /// fails if `Tree` builds a path by joining the wrong segments, or in
    /// the wrong order. `Tree` is the one place allowed to know this
    /// layout, so a mistake here is a mistake everywhere.
    #[test]
    fn every_path_matches_the_documented_layout() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "bpm");
        let remote = "https://github.com/shep-pm/koji";
        let build = BuildKey::of(Some("make"), None, &BTreeMap::new());
        let key = build.as_str().to_owned();

        assert_eq!(tree.git(), Path::new("/srv/shep/deploy/bpm/git"));
        assert_eq!(
            tree.repo(remote).releases(),
            Path::new("/srv/shep/deploy/repos")
                .join(repo_tail(remote))
                .join("releases")
        );
        assert_eq!(
            tree.release(remote, "a1b2c3d", &build),
            Path::new("/srv/shep/deploy/repos")
                .join(repo_tail(remote))
                .join("releases")
                .join(format!("a1b2c3d-{key}"))
        );
        assert_eq!(
            tree.completion(remote, "a1b2c3d", &build),
            Path::new("/srv/shep/deploy/repos")
                .join(repo_tail(remote))
                .join("complete")
                .join(format!("a1b2c3d-{key}"))
        );
        assert_eq!(tree.current(), Path::new("/srv/shep/deploy/bpm/current"));
        assert_eq!(
            tree.superseded_releases(),
            Path::new("/srv/shep/deploy/bpm/releases")
        );
        assert_eq!(
            tree.superseded_completions(),
            Path::new("/srv/shep/deploy/bpm/complete")
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

    /// The `<key>` directory a remote names, spelled the way `Repo` spells it,
    /// so the layout test above is not just a copy of the code.
    fn repo_tail(remote: &str) -> String {
        Repo::for_remote(Path::new("/srv/shep"), remote)
            .root()
            .file_name()
            .expect("a key")
            .to_string_lossy()
            .into_owned()
    }

    /// fails if a `git` link that points outside this deploy directory's
    /// `repos/` is taken for a shared clone. Letting go of one removes its
    /// directory, so a link written by hand to `/data/repos/x/git` would have
    /// `/data/repos/x` removed.
    #[test]
    fn only_a_link_into_this_deploy_dirs_repos_is_a_shared_clone() {
        let home = tempfile::tempdir().expect("tempdir");
        let tree = Tree::for_sheep(home.path(), "web");
        std::fs::create_dir_all(tree.root()).expect("tree");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let outside = elsewhere.path().join("repos").join("x").join("git");
        std::os::unix::fs::symlink(&outside, tree.git()).expect("link");
        assert_eq!(tree.linked_clone(), None, "outside");

        std::fs::remove_file(tree.git()).expect("unlink");
        let repo = Repo::for_remote(home.path(), "https://example.com/x");
        tree.link_clone(&repo).expect("link");
        assert_eq!(tree.linked_clone(), Some(repo), "inside");
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

    /// fails if the cache moves out of `repos/<key>/` or changes name. Every
    /// release symlinks `target` at this one path, so a release built against
    /// a cache at one location and a later release linking another silently
    /// loses every incremental artifact between them, which reads as "the
    /// build is just slow today".
    #[test]
    fn the_build_cache_lives_beside_the_shared_clone() {
        let remote = "https://github.com/shep-pm/koji";
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        let (repo, target) = (tree.repo(remote), tree.cache_target(remote));

        assert_eq!(repo.cache(), repo.root().join("cache"));
        assert_eq!(target, repo.cache().join("target"));

        let rel = target
            .strip_prefix("/srv/shep/deploy/repos")
            .expect("under repos");
        let parts: Vec<_> = rel.components().collect();
        assert_eq!(parts.len(), 3, "<key>/cache/target, got {rel:?}");
        assert_eq!(parts[1].as_os_str(), "cache");
        assert_eq!(parts[2].as_os_str(), "target");
        assert!(
            matches!(parts[0], Component::Normal(_)),
            "key is an ordinary component: {rel:?}"
        );
    }

    /// fails if two sheep of one remote build against different caches, or
    /// two remotes against one. `bpm`, `ctm` and `opm` off one checkout are
    /// one repository, and building it once per sheep is what this placement
    /// exists to remove.
    #[test]
    fn two_sheep_of_one_remote_share_one_build_cache() {
        let home = Path::new("/srv/shep");
        let remote = "https://github.com/shep-pm/ReactMap";
        let shared = Tree::for_sheep(home, "bpm").cache_target(remote);

        assert_eq!(Tree::for_sheep(home, "ctm").cache_target(remote), shared);
        assert_eq!(Tree::for_sheep(home, "opm").cache_target(remote), shared);
        assert_ne!(
            Tree::for_sheep(home, "bpm").cache_target("https://github.com/shep-pm/other"),
            shared,
            "another remote is another cache"
        );
    }

    /// fails if the cache lands inside a tree after all. A tree is per sheep,
    /// and a cache in one is per sheep too: every sheep of one remote would
    /// rebuild the same tree on its own, with nothing anywhere saying so.
    #[test]
    fn the_build_cache_is_not_inside_a_tree() {
        let tree = Tree::for_sheep(Path::new("/srv/shep"), "koji");
        let cache = tree.repo("https://github.com/shep-pm/koji").cache();

        assert!(!cache.starts_with(tree.root()), "{cache:?}");
        assert!(
            !cache.starts_with(tree.repo("https://github.com/shep-pm/koji").releases()),
            "{cache:?}"
        );
    }

    /// fails if a tree that owns its clone stops sharing the cache. The cache
    /// is keyed by remote, not by which clone a tree uses, so a tree set up
    /// before clones were shared warms the same cache as the trees linked to
    /// the shared one.
    #[test]
    fn a_tree_that_owns_its_clone_shares_the_remote_cache() {
        let home = tempfile::tempdir().expect("tempdir");
        let remote = "https://example.com/koji";
        let owned = Tree::for_sheep(home.path(), "koji");
        std::fs::create_dir_all(owned.git()).expect("a clone of its own");
        let sibling = Tree::for_sheep(home.path(), "koji-staging");

        assert!(owned.owns_clone());
        assert_eq!(owned.cache_target(remote), sibling.cache_target(remote));
        assert!(!owned.cache_target(remote).starts_with(owned.root()));
    }
}
