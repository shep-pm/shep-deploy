use crate::fixtures;

use super::*;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::time::Duration;

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
    let (tree, shas) = fixture_tree_with_releases(3);
    let stray = tree.releases().join("notes.txt");
    fs::write(&stray, "not a release").expect("stray");

    at_second(&tree.release(&shas[0]), 10);
    at_second(&tree.release(&shas[1]), 20);
    at_second(&tree.release(&shas[2]), 30);
    at_second(&stray, 40);

    prune(&tree, 2, &[]).expect("prunes");

    assert!(
        tree.release(&shas[1]).is_dir(),
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
    let (tree, shas) = fixture_tree_with_releases(4);
    let never_a_worktree = tree.releases().join("0".repeat(40));
    fs::create_dir(&never_a_worktree).expect("a release-shaped directory");

    at_second(&tree.release(&shas[0]), 10);
    at_second(&tree.release(&shas[1]), 20);
    at_second(&never_a_worktree, 30);
    at_second(&tree.release(&shas[2]), 40);
    at_second(&tree.release(&shas[3]), 50);

    let err = prune(&tree, 2, &[]).expect_err("the failure must still be reported");

    assert!(
        !tree.release(&shas[0]).exists() && !tree.release(&shas[1]).exists(),
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

/// A real bare repo with `n` commits, a worktree per commit under
/// `tree.releases()` in commit order, each written into so it is dirty
/// the way a built release is, with `current` pointed at the newest.
///
/// Returns the shas in commit order, oldest first, so `shas[n - 1]` is
/// always the one `current` names.
fn fixture_tree_with_releases(n: u32) -> (Tree, Vec<String>) {
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

    for sha in &shas {
        let release = tree.release(sha);
        git::worktree_add(&tree.git(), &release, sha).expect("worktree add");
        fs::write(release.join("built.txt"), "built").expect("dirty the release");
    }
    swap::point_at(
        &tree.current(),
        &tree.release(shas.last().expect("at least one")),
    )
    .expect("point current");

    // Leaked deliberately: `prune` and its assertions run against real
    // paths on disk after this function returns, and an auto-cleaned
    // `TempDir` would delete them out from under the test.
    let _ = origin.keep();
    let _ = home.keep();

    (tree, shas)
}

/// fails if retention stops keeping the newest `keep`. Everything else
/// in this module is a consequence of getting this ordering right, and
/// getting it backwards would delete the live release and keep the
/// ancient ones.
#[test]
fn the_newest_releases_survive() {
    let all = releases(&["new", "old", "older", "ancient"]);
    assert_eq!(doomed(&all, 2, &[None]), vec!["older", "ancient"]);
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
    let (tree, shas) = fixture_tree_with_releases(2);
    let orphan = tree.release(&shas[0]);
    fs::remove_dir_all(&orphan).expect("remove the release by hand");

    // keep is larger than the release count, so nothing is past it and
    // `removed` comes back empty. That is the case the gate broke.
    let removed = prune(&tree, 5, &[]).expect("prune");
    assert!(removed.is_empty(), "nothing was past the keep count");

    git::worktree_add(&tree.git(), &orphan, &shas[0])
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
    assert_eq!(doomed(&all, 2, &[Some("ancient")]), vec!["older"]);
    // At keep = 1 the live release is the ONLY survivor besides the
    // newest, which is the "whatever its age" the name promises. Asserting
    // merely that `ancient` is absent restated `doomed`'s unconditional
    // live filter and would have passed at any keep, including a keep this
    // function does not honour.
    assert_eq!(doomed(&all, 1, &[Some("ancient")]), vec!["old", "older"]);
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
        .filter(|name| !doomed(&all, 2, &[None]).contains(name))
        .collect();
    assert_eq!(survivors, vec!["new", "old"]);
}

/// fails if a tree with fewer releases than the retention count starts
/// removing things. The first few deploys of every target are this
/// case.
#[test]
fn a_young_tree_loses_nothing() {
    assert!(doomed(&releases(&["new", "old"]), 5, &[None]).is_empty());
    assert!(doomed(&[], 5, &[None]).is_empty());
}

/// fails if `prune` stops actually removing directories, or removes
/// something it should not. The pure function above pins the decision;
/// this pins that the decision is carried out against real worktrees,
/// which is where `--force` matters: a built worktree is always dirty.
#[test]
fn prune_removes_real_worktrees_and_leaves_the_live_one() {
    let (tree, shas) = fixture_tree_with_releases(4);
    let removed = prune(&tree, 2, &[]).expect("prunes");

    assert_eq!(removed.len(), 2);
    assert!(tree.release(&shas[3]).exists(), "the newest survives");
    assert!(
        tree.release(&shas[2]).exists(),
        "so does the rollback target"
    );
    assert!(!tree.release(&shas[0]).exists(), "the oldest is gone");
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
        doomed(&all, 1, &[Some("newest"), Some("recorded")]),
        vec!["older", "ancient"]
    );
}

/// fails if a removed release leaves its completion marker behind, or a
/// marker whose release vanished by other means is never swept.
/// Nothing else removes one, so the tree used to gain a zero-byte file
/// per commit ever deployed, forever.
#[test]
fn a_reclaimed_release_takes_its_completion_marker_with_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let tree = Tree::for_sheep(home.path(), "web");
    let origin = fixtures::checkout(&[("f", "x")]);
    let sha = fixtures::head_of(origin.path());
    git::init_bare(&tree.git()).expect("bare");
    git::fetch(
        &tree.git(),
        origin.path().to_str().expect("utf-8"),
        "main",
        fixtures::TEST_BUDGET,
    )
    .expect("fetch");
    fs::create_dir_all(tree.releases()).expect("releases");

    // Three releases named by real-looking shas, the oldest being the
    // one real worktree; the two newer ones are stand-ins that keep it
    // out of the kept set.
    let doomed_sha = sha.clone();
    git::worktree_add(&tree.git(), &tree.release(&doomed_sha), &sha).expect("worktree");
    at_second(&tree.release(&doomed_sha), 1);
    for (n, stand_in) in ["1".repeat(40), "2".repeat(40)].iter().enumerate() {
        fs::create_dir_all(tree.release(stand_in)).expect("stand-in");
        at_second(&tree.release(stand_in), 10 + n as u64);
    }
    let markers = tree
        .completion(&doomed_sha)
        .parent()
        .expect("complete/")
        .to_owned();
    fs::create_dir_all(&markers).expect("complete dir");
    fs::write(tree.completion(&doomed_sha), b"").expect("marker");
    let orphan = tree.completion(&"3".repeat(40));
    fs::write(&orphan, b"").expect("a marker with no release");

    let removed = prune(&tree, 2, &[]).expect("prunes");

    assert_eq!(removed, vec![doomed_sha.clone()]);
    assert!(
        !tree.completion(&doomed_sha).exists(),
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

    fs::create_dir_all(repo.cache_target()).expect("cache");
    fs::write(repo.cache_target().join("artifact"), "warm").expect("warm cache");
    fs::create_dir_all(own.release(fixtures::SHA)).expect("release");
    symlink(
        repo.cache_target(),
        own.release(fixtures::SHA).join("target"),
    )
    .expect("the release's link at the cache");

    assert!(release_clone(home.path(), &linked).expect("releases"));

    assert!(!repo.git().exists(), "no tree links to the clone any more");
    assert!(
        repo.cache_target().join("artifact").is_file(),
        "the cache the other tree builds against stays, with what is in it"
    );
    assert!(
        fs::canonicalize(own.release(fixtures::SHA).join("target")).is_ok(),
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

/// fails if retention removes a shared clone while pruning a release.
/// It reclaims worktrees and nothing else, and a clone belongs to every
/// tree linking to it.
#[test]
fn pruning_a_tree_never_removes_the_shared_clone() {
    let (home, web, _api, repo) = two_trees_sharing_a_clone();
    fs::create_dir_all(web.releases()).expect("releases");

    prune(&web, 2, &[]).expect("prunes");

    assert!(repo.git().join("HEAD").is_file());
    assert!(web.git().join("HEAD").is_file());
    drop(home);
}
