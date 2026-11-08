use crate::fixtures;

use super::*;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::time::Duration;

/// One tree plus the remote its releases are keyed by.
///
/// Every path this module reclaims is now a repository's rather than a tree's,
/// so a test needs the remote alongside the tree it is pruning. The helpers
/// here answer the paths a test asserts on with the default settings'
/// [`BuildKey`], which is what every fixture in this file builds with.
struct Target {
    tree: Tree,
    remote: String,
}

impl Target {
    /// The directory `sha`'s release lives in.
    fn release(&self, sha: &str) -> PathBuf {
        self.tree
            .release(&self.remote, sha, &fixtures::default_build_key())
    }

    /// That release's completion marker.
    fn completion(&self, sha: &str) -> PathBuf {
        self.tree
            .completion(&self.remote, sha, &fixtures::default_build_key())
    }

    /// The directory every release of this remote lives in.
    fn releases(&self) -> PathBuf {
        self.tree.repo(&self.remote).releases()
    }

    /// The directory their markers live in.
    fn completions(&self) -> PathBuf {
        self.tree.repo(&self.remote).completions()
    }

    /// This tree's prune of the repository it deploys from.
    fn prune(&self, keep: usize, recorded: &[&str]) -> Result<Vec<String>, Error> {
        prune(&self.tree, &self.remote, keep, recorded)
    }
}

/// A [`Spared`] holding `shas` as what records name, for the tests of
/// [`doomed`] that have no filesystem to read.
fn spared_by(shas: &[&str]) -> Spared {
    Spared {
        names: BTreeSet::new(),
        shas: shas.iter().map(|sha| (*sha).to_owned()).collect(),
    }
}

/// A [`Spared`] holding `names` as release directory names a tree's `current`
/// resolves to. The other half of [`Spared`], and the half [`prune`] answers
/// for a sheep whose record it never reads.
fn spared_names(names: &[&str]) -> Spared {
    Spared {
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        shas: BTreeSet::new(),
    }
}

/// Pins `path`'s modification time, so an ordering test does not depend
/// on how far apart two filesystem operations happen to land.
///
/// Both tests below did depend on that, and CI caught it: on macOS the
/// gaps were wide enough, on ubuntu two entries tied and `doomed`'s
/// tiebreak is the name ascending, which put a `0`-prefixed stand-in in
/// the KEPT group instead of the doomed one. The test then passed against
/// the very bug it was written for.
fn at_second(path: &Path, seconds: u64) {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
    fs::File::open(path)
        .expect("open for set_times")
        .set_times(fs::FileTimes::new().set_modified(when))
        .expect("pin the mtime");
}

/// fails if something under `releases/` that is not a release can take
/// one of the kept slots.
///
/// `prune` read every directory entry's name and mtime and ordered them
/// together, with no check that an entry was a release at all. A stray
/// file written more recently than the releases therefore sorted to the
/// front, held a `keep` slot, and pushed a real release one place down
/// into the doomed set.
///
/// The one it pushes out is the second newest, which is the release a
/// failed deploy rolls back to. `retention` refuses a `keep` below 2 to
/// guarantee that release exists; this deleted it anyway.
#[test]
fn a_stray_file_under_releases_cannot_push_a_release_out() {
    let (target, shas) = fixture_tree_with_releases(3);
    let stray = target.releases().join("notes.txt");
    fs::write(&stray, "not a release").expect("stray");

    at_second(&target.release(&shas[0]), 10);
    at_second(&target.release(&shas[1]), 20);
    at_second(&target.release(&shas[2]), 30);
    at_second(&stray, 40);

    target.prune(2, &[]).expect("prunes");

    assert!(
        target.release(&shas[1]).is_dir(),
        "the second newest release is the rollback target and must survive"
    );
}

/// fails if one release that will not go stops the rest of the cycle.
///
/// The removal loop used a bare `?`, so the first failure returned
/// immediately: every later doomed release stayed, and the
/// `worktree_prune` below it never ran, though the comment there says it
/// must run unconditionally. Nothing self-corrected either, because
/// whatever made the removal fail was still there next cycle.
///
/// The stand-in is a sha-named directory that was never a worktree, which
/// is what a kill during `git worktree add` leaves behind and what an
/// operator copying a release by hand makes. It passes the
/// is-this-a-release filter, because it is shaped exactly like one, and
/// `git worktree remove` then refuses it.
///
/// Its mtime is pinned between the kept releases and the doomed ones so
/// it is removed FIRST. Ordered last it would strand nothing and the test
/// would pass against the unfixed code.
#[test]
fn one_release_that_will_not_go_does_not_strand_the_others() {
    let (target, shas) = fixture_tree_with_releases(4);
    let never_a_worktree = target.releases().join("0".repeat(40));
    fs::create_dir(&never_a_worktree).expect("a release-shaped directory");

    at_second(&target.release(&shas[0]), 10);
    at_second(&target.release(&shas[1]), 20);
    at_second(&never_a_worktree, 30);
    at_second(&target.release(&shas[2]), 40);
    at_second(&target.release(&shas[3]), 50);

    let err = target
        .prune(2, &[])
        .expect_err("the failure must still be reported");

    assert!(
        !target.release(&shas[0]).exists() && !target.release(&shas[1]).exists(),
        "the rest of the cycle must run past the failure: {err}"
    );
}

/// Three releases, newest first by the second field.
fn releases(names: &[&str]) -> Vec<(String, SystemTime)> {
    let base = SystemTime::UNIX_EPOCH;
    names
        .iter()
        .enumerate()
        .map(|(age, name)| {
            (
                (*name).to_owned(),
                base + Duration::from_secs(1000 - age as u64),
            )
        })
        .collect()
}

/// A real bare repo with `n` commits, a worktree per commit under the
/// repository's `releases/` in commit order, each written into so it is dirty
/// the way a built release is, with `current` pointed at the newest.
///
/// Returns the shas in commit order, oldest first, so `shas[n - 1]` is
/// always the one `current` names.
fn fixture_tree_with_releases(n: u32) -> (Target, Vec<String>) {
    let origin = fixtures::empty_checkout();
    for i in 0..n {
        let name = format!("file-{i}.txt");
        fixtures::commit(origin.path(), &[(&name, "x")], &format!("commit {i}"));
    }

    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "web");
    fs::create_dir_all(tree.git()).expect("create git dir");
    fixtures::run_git(&tree.git(), &["init", "-q", "--bare"]);
    let remote = origin.path().to_str().expect("utf-8 path").to_owned();
    git::fetch(&tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");

    let log = Command::new("git")
        .current_dir(tree.git())
        .args(["log", "--reverse", "--format=%H", "main"])
        .output()
        .expect("log");
    let shas: Vec<String> = String::from_utf8(log.stdout)
        .expect("utf-8 log")
        .lines()
        .map(str::to_owned)
        .collect();

    let target = Target {
        tree: tree.clone(),
        remote: remote.clone(),
    };
    for sha in &shas {
        let release = target.release(sha);
        git::worktree_add(&tree.git(), &release, sha).expect("worktree add");
        fs::write(release.join("built.txt"), "built").expect("dirty the release");
    }
    swap::point_at(
        &tree.current(),
        &target.release(shas.last().expect("at least one")),
    )
    .expect("point current");

    // Leaked deliberately: `prune` and its assertions run against real
    // paths on disk after this function returns, and an auto-cleaned
    // `TempDir` would delete them out from under the test.
    let _ = origin.keep();
    let _ = home.keep();

    (target, shas)
}

/// fails if retention stops keeping the newest `keep`. Everything else
/// in this module is a consequence of getting this ordering right, and
/// getting it backwards would delete the live release and keep the
/// ancient ones.
#[test]
fn the_newest_releases_survive() {
    let all = releases(&["new", "old", "older", "ancient"]);
    assert_eq!(doomed(&all, 2, &spared_by(&[])), vec!["older", "ancient"]);
}

/// fails if `prune` only clears git's stale worktree bookkeeping on
/// cycles that removed something themselves. `worktree_prune` exists for
/// the directory that vanished by some other means, an operator's own
/// `rm -rf` or a crash mid-build, and gating it on our own removals
/// stopped it running in exactly that case. The sha then stayed
/// registered, and redeploying it failed with "is a missing but already
/// registered worktree".
#[test]
fn a_release_removed_by_hand_is_deregistered_even_when_nothing_is_pruned() {
    let (target, shas) = fixture_tree_with_releases(2);
    let orphan = target.release(&shas[0]);
    fs::remove_dir_all(&orphan).expect("remove the release by hand");

    // keep is larger than the release count, so nothing is past it and
    // `removed` comes back empty. That is the case the gate broke.
    let removed = target.prune(5, &[]).expect("prune");
    assert!(removed.is_empty(), "nothing was past the keep count");

    git::worktree_add(&target.tree.git(), &orphan, &shas[0])
        .expect("the sha redeploys after prune cleared its registration");
    assert!(orphan.is_dir(), "the release is back on disk");
}

/// fails if a release still named by `current` can be pruned. Removing
/// it leaves the sheep with a `cwd` that resolves to nothing, so the
/// next restart cannot start it at all, and the deploy that caused it
/// reported success minutes earlier.
#[test]
fn the_live_release_is_never_pruned_whatever_its_age() {
    let all = releases(&["new", "old", "older", "ancient"]);
    assert_eq!(doomed(&all, 2, &spared_names(&["ancient"])), vec!["older"]);
    // At keep = 1 the live release is the ONLY survivor besides the
    // newest, which is the "whatever its age" the name promises. Asserting
    // merely that `ancient` is absent restated `doomed`'s unconditional
    // live filter and would have passed at any keep, including a keep this
    // function does not honour.
    assert_eq!(
        doomed(&all, 1, &spared_names(&["ancient"])),
        vec!["old", "older"]
    );
}

/// fails if the rollback target is pruned along with the rest. The
/// second newest release IS what a failed deploy returns to, and
/// `config` refuses a retention below two for this reason, so this
/// pins the other end of the same rule.
#[test]
fn keeping_two_leaves_a_rollback_target() {
    let all = releases(&["new", "old", "older"]);
    let survivors: Vec<String> = all
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| !doomed(&all, 2, &spared_by(&[])).contains(name))
        .collect();
    assert_eq!(survivors, vec!["new", "old"]);
}

/// fails if a tree with fewer releases than the retention count starts
/// removing things. The first few deploys of every target are this
/// case.
#[test]
fn a_young_tree_loses_nothing() {
    assert!(doomed(&releases(&["new", "old"]), 5, &spared_by(&[])).is_empty());
    assert!(doomed(&[], 5, &spared_by(&[])).is_empty());
}

/// fails if `prune` stops actually removing directories, or removes
/// something it should not. The pure function above pins the decision;
/// this pins that the decision is carried out against real worktrees,
/// which is where `--force` matters: a built worktree is always dirty.
#[test]
fn prune_removes_real_worktrees_and_leaves_the_live_one() {
    let (target, shas) = fixture_tree_with_releases(4);
    let removed = target.prune(2, &[]).expect("prunes");

    assert_eq!(removed.len(), 2);
    assert!(target.release(&shas[3]).exists(), "the newest survives");
    assert!(
        target.release(&shas[2]).exists(),
        "so does the rollback target"
    );
    assert!(!target.release(&shas[0]).exists(), "the oldest is gone");
}

/// fails if the release `deploy.toml` names can be reclaimed while
/// `current` names something else.
///
/// The two disagree after a deploy killed between its swap and its record
/// write, which is exactly when the recorded release is what the next
/// rollback returns to, and that release need not be the newest by
/// mtime. Sparing `current` alone let a run of failed shas with fresh
/// mtimes push it out, and the next failure then had nothing to roll
/// back to.
#[test]
fn the_recorded_release_is_spared_like_the_live_one() {
    let all = releases(&["newest", "recorded", "older", "ancient"]);
    assert_eq!(
        doomed(&all, 1, &spared_by(&["newest", "recorded"])),
        vec!["older", "ancient"]
    );
}

/// fails if a record spares every build of the sha it names, not only the
/// one it was built by.
///
/// A record carries a sha and no build key, so it cannot name the directory
/// exactly. The wider answer is the safe one: a stale variant costs disk, and
/// a live one removed costs a running service its working directory.
#[test]
fn a_record_spares_every_build_of_the_sha_it_names() {
    let (first, second) = (fixtures::SHA, fixtures::OTHER_SHA);
    let all = releases(&[
        &format!("{first}-0000000000000000"),
        &format!("{first}-1111111111111111"),
        &format!("{second}-0000000000000000"),
    ]);
    assert_eq!(
        doomed(&all, 2, &spared_by(&[first])),
        vec![format!("{second}-0000000000000000")],
        "both of {first}'s builds outlive the sha past the keep count"
    );
}

/// fails if a removed release leaves its completion marker behind, or a
/// marker whose release vanished by other means is never swept.
/// Nothing else removes one, so the repository used to gain a zero-byte
/// file per commit ever deployed, forever.
#[test]
fn a_reclaimed_release_takes_its_completion_marker_with_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "web");
    let origin = fixtures::checkout(&[("f", "x")]);
    let sha = fixtures::head_of(origin.path());
    let remote = origin.path().to_str().expect("utf-8").to_owned();
    git::init_bare(&tree.git()).expect("bare");
    git::fetch(&tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");
    let target = Target {
        tree: tree.clone(),
        remote: remote.clone(),
    };
    fs::create_dir_all(target.releases()).expect("releases");

    // Three releases named by real-looking shas, the oldest being the
    // one real worktree; the two newer ones are stand-ins that keep it
    // out of the kept set.
    let doomed = target.release(&sha);
    git::worktree_add(&tree.git(), &doomed, &sha).expect("worktree");
    at_second(&doomed, 1);
    for (n, stand_in) in ["1".repeat(40), "2".repeat(40)].iter().enumerate() {
        let other = target.release(stand_in);
        fs::create_dir_all(&other).expect("stand-in");
        at_second(&other, 10 + n as u64);
    }
    fs::create_dir_all(target.completions()).expect("complete dir");
    fs::write(target.completion(&sha), b"").expect("marker");
    let orphan = target.completion(&"3".repeat(40));
    fs::write(&orphan, b"").expect("a marker with no release");

    let removed = target.prune(2, &[]).expect("prunes");

    let name = doomed
        .file_name()
        .expect("a release name")
        .to_string_lossy();
    assert_eq!(removed, vec![name.to_string()]);
    assert!(
        !target.completion(&sha).exists(),
        "the reclaimed release's marker must go with it"
    );
    assert!(!orphan.exists(), "a marker with no release must be swept");
}

/// Two trees linked to one shared clone, as setup leaves them.
fn two_trees_sharing_a_clone() -> (tempfile::TempDir, Tree, Tree, Repo) {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = Repo::for_remote(home.path(), "https://example.com/x.git");
    git::init_bare(&repo.git()).expect("bare");
    let trees = ["web", "api"].map(|sheep| {
        let tree = Tree::for_sheep(home.path(), sheep);
        fs::create_dir_all(tree.root()).expect("tree");
        tree.link_clone(&repo).expect("link");
        tree
    });
    let [web, api] = trees;
    (home, web, api, repo)
}

/// fails if removing one of two sheep takes the clone the other still
/// links to, or if removing the second leaves it behind. The first is
/// the other sheep's next deploy failing on a clone that is gone; the
/// second is the object store of a removed sheep kept forever.
#[test]
fn the_clone_goes_with_the_last_tree_linking_to_it() {
    let (home, web, api, repo) = two_trees_sharing_a_clone();

    assert!(!release_clone(home.path(), &web).expect("first"));
    assert!(repo.git().join("HEAD").is_file(), "api still links to it");
    assert!(web.git().symlink_metadata().is_err(), "web's link is gone");
    assert!(api.git().join("HEAD").is_file(), "api still reaches it");

    fs::create_dir_all(repo.cache_target()).expect("cache");
    fs::write(repo.cache_target().join("artifact"), "warm").expect("warm cache");

    assert!(release_clone(home.path(), &api).expect("second"));
    assert!(!repo.root().exists(), "nobody links to it now");
    assert!(
        !repo.cache_target().exists(),
        "and the build cache goes with the last tree of its remote"
    );
}

/// fails if a tree that was linked to a clone but has no record yet, a
/// setup that has not finished, is not counted as a link. Its setup
/// would be fetching into a clone removed beneath it.
#[test]
fn a_tree_with_no_record_yet_still_counts_as_a_link() {
    let (home, web, api, repo) = two_trees_sharing_a_clone();
    assert!(!api.state_file().exists(), "api has no deploy.toml");

    assert!(!release_clone(home.path(), &web).expect("releases"));

    assert!(repo.git().exists());
}

/// fails if releasing a clone a tree owns touches it. A tree from before
/// shared clones keeps its own, and removing it is not this module's.
#[test]
fn a_tree_with_its_own_clone_is_left_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "web");
    git::init_bare(&tree.git()).expect("bare");

    assert!(!release_clone(home.path(), &tree).expect("nothing to release"));

    assert!(tree.git().join("HEAD").is_file());
}

/// One tree linked to the shared clone for `remote`, and a second tree under
/// the same home, whose clone and record each test below writes itself.
fn a_linked_tree_and_one_other(remote: &str) -> (tempfile::TempDir, Tree, Tree, Repo) {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = Repo::for_remote(home.path(), remote);
    git::init_bare(&repo.git()).expect("bare");

    let linked = Tree::for_sheep(home.path(), "linked");
    fs::create_dir_all(linked.root()).expect("tree");
    linked.link_clone(&repo).expect("link");

    let other = Tree::for_sheep(home.path(), "other");
    fs::create_dir_all(other.root()).expect("tree");
    (home, linked, other, repo)
}

/// fails if letting go of a shared clone takes the build cache a tree that
/// owns its own clone is building against.
///
/// `crate::optin::clone::share_clone` never migrates a tree that has a
/// record, so a legacy tree keeps its clone and still links every release's
/// `target` into the repository cache. Removing a different sheep of the same
/// remote used to remove that whole directory: the live release's
/// `current/target` became a dangling link, so the app lost
/// `./target/release/<bin>` at its next restart, and a build of that tree
/// running at that moment lost its cache.
#[test]
fn a_released_clone_leaves_the_cache_of_a_tree_that_owns_its_clone() {
    let remote = "https://example.com/x.git";
    let (home, linked, own, repo) = a_linked_tree_and_one_other(remote);
    git::init_bare(&own.git()).expect("its own clone");
    State {
        remote: remote.to_owned(),
        ..fixtures::state()
    }
    .write(&own.state_file())
    .expect("record");
    let own_release = Target {
        tree: own.clone(),
        remote: remote.to_owned(),
    }
    .release(fixtures::SHA);

    fs::create_dir_all(repo.cache_target()).expect("cache");
    fs::write(repo.cache_target().join("artifact"), "warm").expect("warm cache");
    fs::create_dir_all(&own_release).expect("release");
    symlink(repo.cache_target(), own_release.join("target"))
        .expect("the release's link at the cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(!repo.git().exists(), "no tree links to the clone any more");
    assert!(
        repo.cache_target().join("artifact").is_file(),
        "the cache the other tree builds against stays, with what is in it"
    );
    assert!(
        fs::canonicalize(own_release.join("target")).is_ok(),
        "and the live release's link still resolves"
    );
}

/// fails if a record naming some other remote keeps this remote's cache. The
/// question is which remote a tree deploys, not whether it deploys anything.
#[test]
fn a_tree_of_another_remote_does_not_keep_the_cache() {
    let (home, linked, other, repo) = a_linked_tree_and_one_other("https://example.com/x.git");
    git::init_bare(&other.git()).expect("its own clone");
    State {
        remote: "https://example.com/y.git".to_owned(),
        ..fixtures::state()
    }
    .write(&other.state_file())
    .expect("record");
    fs::create_dir_all(repo.cache_target()).expect("cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(
        !repo.root().exists(),
        "the whole repository directory goes with the last tree of its remote"
    );
}

/// fails if a record that cannot be read counts as no record at all. The
/// question is whether anything is still building against the cache, and a
/// tree with a broken `deploy.toml` is still a tree.
#[test]
fn a_tree_with_an_unreadable_record_keeps_the_cache() {
    let (home, linked, broken, repo) = a_linked_tree_and_one_other("https://example.com/x.git");
    git::init_bare(&broken.git()).expect("its own clone");
    fs::write(broken.state_file(), "this is not toml").expect("record");
    fs::create_dir_all(repo.cache_target()).expect("cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(!repo.git().exists(), "the clone still goes");
    assert!(repo.cache_target().is_dir(), "the cache does not");
}

/// Two sheep of one remote, as a deploy leaves them: each linked to the
/// shared clone for that remote, each with a record naming it, and the
/// repository holding three built releases every one of them links
/// `target` into the one build cache it holds.
///
/// The shas come back oldest first, so `shas.last()` is what both `current`
/// links name.
fn two_sheep_sharing_a_build() -> (tempfile::TempDir, Tree, Tree, Repo, Vec<String>) {
    let origin = fixtures::empty_checkout();
    let mut shas = Vec::new();
    for i in 0..3 {
        fixtures::commit(
            origin.path(),
            &[(&format!("file-{i}.txt"), "x")],
            &format!("commit {i}"),
        );
        shas.push(fixtures::head_of(origin.path()));
    }
    let remote = origin.path().to_str().expect("utf-8").to_owned();

    let home = tempfile::tempdir().expect("tempdir");
    let repo = Repo::for_remote(home.path(), &remote);
    git::init_bare(&repo.git()).expect("bare");
    git::fetch(&repo.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");
    fs::create_dir_all(repo.cache_target()).expect("cache");
    fs::write(repo.cache_target().join("artifact"), "warm").expect("warm cache");

    // One worktree per sha, in the shared directory, built once. Two sheep
    // of one remote do not each check out their own copy of one commit.
    let build = fixtures::default_build_key();
    for sha in &shas {
        let release = repo.release(sha, &build);
        git::worktree_add(&repo.git(), &release, sha).expect("worktree");
        fs::write(release.join("built.txt"), "built").expect("dirty the release");
        symlink(repo.cache_target(), release.join("target"))
            .expect("link the release at the cache");
    }

    let trees = ["web", "api"].map(|sheep| {
        let tree = Tree::for_sheep(home.path(), sheep);
        fs::create_dir_all(tree.root()).expect("tree");
        tree.link_clone(&repo).expect("link");
        git::fetch(&tree.git(), &remote, "main", fixtures::TEST_BUDGET).expect("fetch");
        swap::point_at(
            &tree.current(),
            &repo.release(shas.last().expect("a release"), &build),
        )
        .expect("current");
        State {
            remote: remote.clone(),
            deployed: shas.last().cloned(),
            ..fixtures::state()
        }
        .write(&tree.state_file())
        .expect("record");
        tree
    });

    // Leaked deliberately: every remote in this fixture is a path under
    // this TempDir, and the trees outlive the function.
    let _ = origin.keep();
    let [web, api] = trees;
    (home, web, api, repo, shas)
}

/// fails if pruning one of two sheep of one remote reaches into the other:
/// its releases, or the build cache both compile into.
///
/// A release belongs to the repository, and `prune` is called for one tree at
/// a time. The cache is one directory beside the shared clone, so a cycle that
/// reclaimed it would leave the other sheep serving through a dangling
/// `target` and building from scratch.
#[test]
fn pruning_one_sheep_leaves_the_build_the_other_shares() {
    let (home, web, api, repo, shas) = two_sheep_sharing_a_build();
    let live = shas.last().expect("three releases").clone();
    let remote = State::read(&web.state_file())
        .expect("web's record")
        .remote
        .clone();
    let (web, api) = (
        Target {
            tree: web,
            remote: remote.clone(),
        },
        Target {
            tree: api,
            remote: remote.clone(),
        },
    );

    web.prune(2, &[live.as_str()]).expect("prunes");

    assert!(
        !web.release(&shas[0]).exists(),
        "the oldest release of the remote is reclaimed"
    );
    for sha in &shas[1..] {
        assert!(
            api.release(sha).exists(),
            "and nothing api is serving from: {sha} is still checked out"
        );
    }
    assert!(
        repo.cache_target().join("artifact").is_file(),
        "the build they share stays"
    );
    assert!(
        fs::canonicalize(api.release(&live).join("target")).is_ok(),
        "and api's live release still resolves through it"
    );
    drop(home);
}

/// fails if one sheep's prune deletes the release another sheep is serving
/// from, because that other sheep's `current` is not asked about.
///
/// Releases moved out of the tree and into the repository, so `prune` for `web`
/// is now a sweep over a directory `api` may be serving from. `web`'s own two
/// witnesses - its `current` and its record - say nothing about `api`'s old
/// release, and by mtime it is the one past the keep count. Removing it takes
/// a running service's working directory out from under it.
#[test]
fn pruning_one_sheep_spares_what_another_sheeps_current_names() {
    let (home, web, api, _repo, shas) = two_sheep_sharing_a_build();
    let (oldest, live) = (
        shas.first().expect("three releases").clone(),
        shas.last().expect("three releases").clone(),
    );
    let remote = State::read(&api.state_file())
        .expect("api's record")
        .remote
        .clone();

    // api is still serving the oldest release: its `current` names it and its
    // record says so too. web has moved on to the newest.
    let build = fixtures::default_build_key();
    swap::point_at(
        &api.current(),
        &Repo::for_remote(home.path(), &remote).release(&oldest, &build),
    )
    .expect("api serves the oldest");
    State {
        remote: remote.clone(),
        deployed: Some(oldest.clone()),
        ..fixtures::state()
    }
    .write(&api.state_file())
    .expect("api's record");

    // keep = 2 leaves the two newest by mtime, so the oldest is the candidate.
    let removed = Target {
        tree: web,
        remote: remote.clone(),
    }
    .prune(2, &[live.as_str()])
    .expect("prunes");

    assert!(removed.is_empty(), "nothing may be removed: {removed:?}");
    assert!(
        Repo::for_remote(home.path(), &remote)
            .release(&oldest, &build)
            .is_dir(),
        "api is serving from {oldest}, so web's prune must leave it"
    );
    drop(home);
}

/// fails if letting go of one of two sheep of one remote stops the other
/// deploying, or takes the build it deploys with.
///
/// This is the ordinary case of two sheep off one repository: both linked to
/// one clone, both with records. Removing the first must leave the second
/// with its clone, its releases and its cache, and the build must go only
/// with the last of them.
#[test]
fn removing_one_sheep_leaves_the_other_deploying() {
    let (home, web, api, repo, shas) = two_sheep_sharing_a_build();
    let live = shas.last().expect("three releases").clone();
    let remote = State::read(&api.state_file())
        .expect("api's record")
        .remote
        .clone();
    let api = Target {
        tree: api.clone(),
        remote: remote.clone(),
    };

    assert!(
        !release_clone(home.path(), &web).expect("releases"),
        "api still links to the clone, so nothing goes"
    );
    assert!(web.git().symlink_metadata().is_err(), "web's link is gone");

    assert_eq!(
        git::remote_head(&api.tree.git(), "main").expect("api's branch still resolves"),
        live,
        "api's clone is the same clone it deployed from"
    );
    git::fetch(&api.tree.git(), &remote, "main", fixtures::TEST_BUDGET)
        .expect("api can fetch into it again");
    for sha in &shas {
        assert!(api.release(sha).is_dir(), "api's release {sha} is intact");
    }
    assert!(
        repo.cache_target().join("artifact").is_file(),
        "and so is the build it compiles into"
    );

    assert!(release_clone(home.path(), &api.tree).expect("releases"));
    assert!(
        !repo.root().exists(),
        "the build goes with the last sheep that names it"
    );
}

/// fails if the build goes while a sheep's `current` names it.
///
/// The other sheep here owns its clone and has no record at all, which is
/// what [`crate::optin::prepare_with`] leaves when a run dies between its swap
/// and its record write: `current` names a built release, that release
/// links `target` into the repository's cache, and no record exists to say
/// any of it is deployed. Reading records alone saw nothing and removed the
/// cache, so the live release's `current/target` went dangling and the app
/// lost `./target/release/<bin>` at its next restart.
#[test]
fn a_build_named_by_a_sheeps_current_is_never_removed() {
    let remote = "https://example.com/x.git";
    let (home, linked, own, repo) = a_linked_tree_and_one_other(remote);
    git::init_bare(&own.git()).expect("its own clone");
    assert!(!own.state_file().exists(), "no record was ever written");
    let own_release = Target {
        tree: own.clone(),
        remote: remote.to_owned(),
    }
    .release(fixtures::SHA);

    fs::create_dir_all(&own_release).expect("release");
    fs::write(own_release.join("built.txt"), "built").expect("the release is built");
    symlink(repo.cache_target(), own_release.join("target"))
        .expect("the release's link at the cache");
    swap::point_at(&own.current(), &own_release).expect("current");
    fs::create_dir_all(repo.cache_target()).expect("cache");
    fs::write(repo.cache_target().join("artifact"), "warm").expect("warm cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(!repo.git().exists(), "no tree links to the clone any more");
    assert!(
        repo.cache_target().join("artifact").is_file(),
        "but the build its current links through stays, with what is in it"
    );
    assert!(
        fs::canonicalize(own_release.join("target")).is_ok(),
        "and the live release's link still resolves"
    );
}

/// fails if a sheep that can no longer build keeps the shared build alive.
///
/// The boundary of the rule above, and the half that is deliberate: a sheep
/// that was put back has no clone of its own and no link to one, so it has
/// nothing to build with, and its tree is not serving. Its record and its
/// releases stay, and the build goes with the last sheep that can still use
/// it. See [`another_tree_uses`].
#[test]
fn a_sheep_with_no_clone_and_no_link_keeps_nothing() {
    let remote = "https://example.com/x.git";
    let (home, linked, put_back, repo) = a_linked_tree_and_one_other(remote);
    assert!(
        put_back.git().symlink_metadata().is_err(),
        "its link is gone"
    );
    State {
        remote: remote.to_owned(),
        ..fixtures::state()
    }
    .write(&put_back.state_file())
    .expect("its record stays");
    let leftover = Target {
        tree: put_back.clone(),
        remote: remote.to_owned(),
    }
    .release(fixtures::SHA);

    fs::create_dir_all(&leftover).expect("release");
    symlink(repo.cache_target(), leftover.join("target")).expect("a leftover link at the cache");
    swap::point_at(&put_back.current(), &leftover).expect("current");
    fs::create_dir_all(repo.cache_target()).expect("cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(
        !repo.root().exists(),
        "the whole repository directory goes with the last sheep that can build"
    );
}

/// fails if retention removes a shared clone while pruning a release.
/// It reclaims worktrees and nothing else, and a clone belongs to every
/// tree linking to it.
#[test]
fn pruning_a_tree_never_removes_the_shared_clone() {
    let (home, web, _api, repo) = two_trees_sharing_a_clone();
    fs::create_dir_all(Repo::for_remote(home.path(), "https://example.com/x.git").releases())
        .expect("releases");

    prune(&web, "https://example.com/x.git", 2, &[]).expect("prunes");

    assert!(repo.git().join("HEAD").is_file());
    assert!(web.git().join("HEAD").is_file());
    drop(home);
}
