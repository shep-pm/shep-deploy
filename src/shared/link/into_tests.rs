//! `link_into`, tested at its public seam. Its own file to keep `link.rs`
//! under the IR-48 line limit.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::Mutex;

use super::{FromCheckout, link_into, to_link};
use crate::error::Error;
use crate::fixtures;

/// Guards `link_into_resolves_even_when_checkout_is_relative`, the one
/// test in this module that mutates the process's current directory.
/// `std::env::set_current_dir` is global, process-wide state that
/// Rust's default parallel test runner does nothing to serialise, so a
/// lock is the difference between "one test briefly changes cwd" and
/// "two threads race to change and restore cwd at once".
static CWD_GUARD: Mutex<()> = Mutex::new(());

/// Puts the process's current directory back when dropped, so a panic
/// while it is somewhere else cannot leave every other test there.
struct RestoreCwd(PathBuf);

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        // A failure here cannot be raised from a drop, possibly mid-unwind.
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// fails if `link_into` stops actually linking back to the checkout, or
/// stops creating the parent directories a nested shared path needs.
/// `config/local.json` exercises both: the release has no `config/`
/// directory until `link_into` makes one, and the file it links to must
/// still be the checkout's own copy, not one dragged along.
#[test]
fn link_into_creates_symlinks_that_resolve_into_the_checkout() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", r#"{"real":true}"#),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");

    let paths = to_link(repo.path()).expect("computes");
    link_into(release.path(), repo.path(), &paths).expect("links");

    let linked_path = release.path().join("config").join("local.json");
    assert!(linked_path.is_symlink());
    let contents = fs::read_to_string(&linked_path).expect("read through symlink");
    assert_eq!(contents, r#"{"real":true}"#);
}

/// fails if linking a release a second time stops being accepted. A
/// retry of a held commit reuses the finished release directory, so
/// every link the first attempt made is already there pointing at the
/// very same file, and reporting that as a `.shepignore` collision
/// refused a legitimate shared cache.
#[test]
fn link_into_accepts_links_an_earlier_attempt_already_made() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", r#"{"real":true}"#),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");
    let paths = to_link(repo.path()).expect("computes");

    link_into(release.path(), repo.path(), &paths).expect("first attempt links");
    link_into(release.path(), repo.path(), &paths).expect("second attempt accepts its own links");

    let contents =
        fs::read_to_string(release.path().join("config/local.json")).expect("read through symlink");
    assert_eq!(contents, r#"{"real":true}"#);
}

/// fails if a release-side symlink aimed somewhere else stops being a
/// collision. Only a link to the very target `link_into` would make is
/// the earlier attempt's own; one elsewhere is something the release
/// built for itself, which `.shepignore` has to keep out.
#[test]
fn link_into_refuses_a_link_that_points_elsewhere() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", r#"{"real":true}"#),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");
    let elsewhere = tempfile::tempdir().expect("elsewhere tempdir");
    fs::create_dir_all(release.path().join("config")).expect("config dir");
    symlink(elsewhere.path(), release.path().join("config/local.json")).expect("stray link");
    let paths = to_link(repo.path()).expect("computes");

    let err = link_into(release.path(), repo.path(), &paths)
        .expect_err("a link to another target is a collision");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains(".shepignore"));
}

/// fails if a real file in the release stops being a collision.
#[test]
fn link_into_refuses_a_real_file_in_the_release() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", r#"{"real":true}"#),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");
    fs::create_dir_all(release.path().join("config")).expect("config dir");
    fs::write(release.path().join("config/local.json"), "built").expect("real file");
    let paths = to_link(repo.path()).expect("computes");

    let err =
        link_into(release.path(), repo.path(), &paths).expect_err("a real file is a collision");
    assert!(matches!(err, Error::Config(_)));
}

/// fails if `link_into` goes back to embedding `checkout` literally as
/// the symlink's target text. A relative `checkout` used to produce a
/// symlink whose target the OS resolves against the symlink's own
/// directory inside the release, not against anything the caller
/// meant - `symlink()` itself never noticed, so the only way to catch
/// this is to actually read through the result. cwd is changed to the
/// checkout's own parent so `relative_checkout` is a genuinely relative
/// path the fix must canonicalise, not merely a path that happens to
/// already be absolute.
#[test]
fn link_into_resolves_even_when_checkout_is_relative() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", r#"{"real":true}"#),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");
    let paths = vec![PathBuf::from("config/local.json")];

    let _guard = CWD_GUARD.lock().expect("cwd guard poisoned");
    // Declared after the lock guard, so it drops first: cwd is back before
    // another test can take the lock.
    let _restore = RestoreCwd(std::env::current_dir().expect("read cwd"));
    std::env::set_current_dir(repo.path().parent().expect("repo has a parent"))
        .expect("chdir into repo's parent");
    let relative_checkout = PathBuf::from(repo.path().file_name().expect("repo has a name"));

    link_into(release.path(), &relative_checkout, &FromCheckout::of(paths))
        .expect("links despite a relative checkout");

    let linked_path = release.path().join("config").join("local.json");
    let contents = fs::read_to_string(&linked_path).expect("read through symlink");
    assert_eq!(contents, r#"{"real":true}"#);
}

/// fails if `link_into` stops surfacing a checkout it cannot resolve as
/// an immediate error. Silently doing nothing, or creating a dangling
/// link anyway, is exactly the failure-that-looks-like-success shape
/// the canonicalisation fix exists to close off.
#[test]
fn link_into_fails_loudly_when_checkout_does_not_exist() {
    let release = tempfile::tempdir().expect("release tempdir");
    let missing_checkout = release.path().join("no-such-checkout");
    let paths = vec![PathBuf::from("config/local.json")];

    let err = link_into(release.path(), &missing_checkout, &FromCheckout::of(paths))
        .expect_err("a checkout that does not exist cannot be canonicalised");
    assert!(matches!(err, Error::Io { .. }));
}

/// fails if a directory of a retried release that the previous build swapped
/// for a link carries the dog's writes out of the release. The dog is root and
/// the retried release belongs to the build user, so a path-based
/// `create_dir_all` and `symlink` would follow the link.
#[test]
fn link_into_does_not_write_through_a_directory_swapped_for_a_link() {
    let repo = fixtures::checkout(&[
        (".gitignore", "config/local.json\n"),
        ("config/local.json", "{}"),
    ]);
    let release = tempfile::tempdir().expect("release tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    symlink(outside.path(), release.path().join("config")).expect("swapped directory");

    let paths = to_link(repo.path()).expect("computes");
    let err = link_into(release.path(), repo.path(), &paths)
        .expect_err("a link where a directory belongs must be refused");

    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert_eq!(
        fs::read_dir(outside.path()).expect("lists").count(),
        0,
        "nothing may be written outside the release"
    );
}
