//! Resolving the app's `user` to the uid and primary gid a build drops to,
//! and warning when a build is about to run as root without one.

use std::path::Path;

use tokio::process::Command;

use crate::error::Error;

#[cfg(doc)]
use super::run;

/// Resolves `user` to an id by shelling out to `id <flag> user`, for
/// whichever of `-u` (uid) or `-g` (primary gid) `flag` names. `kind` is
/// only for the error message ("uid" or "gid").
///
/// The crate forbids unsafe code outright, which rules out `getpwnam`
/// directly; asking `id` instead needs none, and it is the same "ask the
/// host rather than reimplement its answer" idiom [`crate::git`] already
/// uses for git itself. It also answers correctly regardless of *how* this
/// host resolves users - `/etc/passwd`, LDAP, anything else NSS is
/// configured for - where a hand-rolled `/etc/passwd` parser would only
/// ever cover the local case.
///
/// Shared by [`uid_for`] and [`gid_for`] rather than duplicated: both need
/// the exact same shape of subprocess call and the exact same two failure
/// modes, and the only thing that differs between them is which flag to
/// pass `id` and which word to use in the message.
///
/// # Errors
/// [`Error::Io`], naming `release`, if `id` itself cannot be launched.
/// [`Error::Config`] if `id <flag>` exits non-zero - most likely because
/// `user` does not exist on this host - or prints something that does not
/// parse as an id.
async fn resolve_id(release: &Path, user: &str, flag: &str, kind: &str) -> Result<u32, Error> {
    let output = Command::new("id")
        .arg(flag)
        .arg(user)
        .output()
        .await
        .map_err(Error::at(release))?;

    if !output.status.success() {
        return Err(Error::Config(format!(
            "the app's `user = {user:?}` does not resolve to a {kind} on this host (`id {flag} \
             {user}` failed); create that account, or pin a different one in \
             Flockfile.override.toml"
        )));
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| Error::Config(format!("`id {flag} {user}` did not print a {kind}")))
}

/// This process's own effective uid, or `None` if it cannot be read.
///
/// Asked of `id -u`, with no user named, for the same reason
/// [`resolve_id`] asks it about somebody else: this crate is
/// `#![forbid(unsafe_code)]`, which rules out `geteuid(2)` directly, and
/// shelling out to the host needs no unsafe and no dependency added for one
/// call.
///
/// `None` rather than an error, and it folds every failure into that: the
/// only caller is a warning, and a deploy that cannot work out its own uid
/// should still deploy. What it loses is the warning, which is the right
/// thing to lose.
async fn effective_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// The warning to print before running `sheep`'s build, or `None` when
/// there is nothing to warn about: [`effective_uid`] and
/// [`root_build_warning`] composed, which is everything [`run`] does with
/// them besides printing.
///
/// The split is where testability runs out and it is worth being exact
/// about where. The decision below is tested at every combination of uid
/// and `user`; this composition is tested against the uid the test host
/// actually has; the `eprintln!` in [`run`] is tested by nothing, because
/// capturing another function's stderr in-process is not something this
/// crate has a harness for. What stands in for a test there is the compile
/// gate: delete the call and this function has no callers, which
/// `-D warnings` refuses. That is weaker than a test and it is the reason
/// this is one function rather than two lines inline.
pub(super) async fn root_warning(sheep: &str, as_user: Option<&str>) -> Option<String> {
    // A build that drops to a user has nothing to warn about, so the `id`
    // spawn is skipped rather than made and then thrown away, which it was
    // on every deploy of every correctly configured app.
    if as_user.is_some() {
        return None;
    }
    root_build_warning(sheep, effective_uid().await?, as_user)
}

/// The warning to print before running `sheep`'s build, or `None` when
/// there is nothing to warn about.
///
/// Fires only when both halves are true: this process is root, and the app
/// sets no `user` for the build to drop to. Then the build - a command the
/// deployed repository chose - runs with the shepherd's own privilege,
/// which is the exposure the module doc describes.
///
/// It warns and never refuses, which is this project owner's explicit
/// decision. An app with no `user` is a configuration shep itself accepts,
/// and a deploy dog is not the place to start declining it.
///
/// Split out from [`run`] so the decision can be tested at every
/// combination without the test having to be root, which is the one thing a
/// test of this cannot arrange.
fn root_build_warning(sheep: &str, euid: u32, as_user: Option<&str>) -> Option<String> {
    const ROOT: u32 = 0;

    (euid == ROOT && as_user.is_none()).then(|| {
        format!(
            "shep-deploy: warning: {sheep}'s build is about to run as root, because its app sets \
             no `user`. The build command comes from the deployed repository, so whatever that \
             repository can be made to run, it runs as root. Set `user` on the app to build as \
             that user instead."
        )
    })
}

/// Resolves `user` to a uid. See [`resolve_id`].
pub(super) async fn uid_for(release: &Path, user: &str) -> Result<u32, Error> {
    resolve_id(release, user, "-u", "uid").await
}

/// Resolves `user` to their *primary* gid. See [`resolve_id`].
///
/// Primary only, deliberately: [`run`]'s `as_user` is a single string, not
/// a user/group pair, so the primary gid `id -g` reports is the only group
/// identity there is anything to derive from here without changing that
/// interface. `shep_core::AppConfig` does carry its own separate `group`
/// field, so an explicit group is available upstream if a later task wants
/// to thread one through - this just doesn't guess at it from `as_user`
/// alone.
pub(super) async fn gid_for(release: &Path, user: &str) -> Result<u32, Error> {
    resolve_id(release, user, "-g", "gid").await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TEST_BUILD_BUDGET;
    use super::super::{BuildSpec, run};
    use super::*;
    use crate::fixtures;

    /// The username running this test process, via `id -un` rather than
    /// the `USER` environment variable - not every CI runner is guaranteed
    /// to set it, and asking the host directly is the same idiom [`uid_for`]
    /// itself uses.
    fn current_username() -> String {
        let output = std::process::Command::new("id")
            .arg("-un")
            .output()
            .expect("id -un runs");
        assert!(output.status.success(), "id -un must succeed");
        String::from_utf8(output.stdout)
            .expect("id -un prints utf-8")
            .trim()
            .to_owned()
    }

    /// This test process's own uid, via bare `id -u` (no username - `id`
    /// defaults to the caller).
    fn current_uid() -> u32 {
        let output = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u runs");
        assert!(output.status.success(), "id -u must succeed");
        String::from_utf8(output.stdout)
            .expect("id -u prints utf-8")
            .trim()
            .parse()
            .expect("id -u prints a number")
    }

    /// As [`current_uid`], for this test process's primary gid.
    fn current_gid() -> u32 {
        let output = std::process::Command::new("id")
            .arg("-g")
            .output()
            .expect("id -g runs");
        assert!(output.status.success(), "id -g must succeed");
        String::from_utf8(output.stdout)
            .expect("id -g prints utf-8")
            .trim()
            .parse()
            .expect("id -g prints a number")
    }

    /// fails if the root warning stops firing on the one combination that
    /// warrants it, or starts firing on one that does not. Both halves have
    /// to be true: this process is root, and the app names no `user` for
    /// the build to drop to.
    ///
    /// Tested as a decision rather than through `run`, because the one
    /// input that matters is this process's uid and a test cannot arrange
    /// to be root.
    #[test]
    fn only_a_rootless_build_as_root_is_warned_about() {
        assert!(root_build_warning("web", 0, None).is_some());

        // Root, but the build drops to somebody: this is the configuration
        // the warning exists to ask for.
        assert!(root_build_warning("web", 0, Some("reactmap")).is_none());
        // Not root: there is no shepherd privilege to hand a build.
        assert!(root_build_warning("web", 501, None).is_none());
        assert!(root_build_warning("web", 501, Some("reactmap")).is_none());
    }

    /// fails if the warning stops naming the sheep, what the exposure is,
    /// or what to do about it. It is the only notice an operator gets, and
    /// a warning that says "running as root" without saying whose code or
    /// which knob is one they learn to skip.
    #[test]
    fn the_root_warning_names_the_sheep_and_the_way_out() {
        let warning = root_build_warning("bpm", 0, None).expect("warns");
        assert!(warning.contains("bpm"), "{warning}");
        assert!(warning.contains("root"), "{warning}");
        assert!(warning.contains("repository"), "{warning}");
        assert!(warning.contains("`user`"), "{warning}");
    }

    /// fails if this process cannot read its own effective uid. The warning
    /// above is skipped entirely when it cannot, so this is what says the
    /// skip is not silently permanent.
    #[tokio::test]
    async fn the_effective_uid_can_be_read() {
        assert!(effective_uid().await.is_some());
    }

    /// fails if reading the uid and deciding on it stop being wired
    /// together. It asserts against whatever uid this host runs tests as
    /// rather than against a fixed answer, so it says the same thing as
    /// root and as anybody else: an app that names a `user` is never warned
    /// about, and an app that does not is warned about exactly when this
    /// process is root.
    #[tokio::test]
    async fn the_warning_reads_this_process_and_agrees_with_the_decision() {
        let euid = effective_uid().await.expect("a uid");

        assert!(root_warning("web", Some("reactmap")).await.is_none());
        assert_eq!(
            root_warning("web", None).await.is_some(),
            euid == 0,
            "warned about as uid {euid}"
        );
    }

    /// fails if `as_user` is silently ignored on the accept side of the
    /// predicate. Dropping to the user already running the process must be
    /// a permitted no-op (`setuid`/`setgid` to one's own ids never require
    /// extra privilege), so this exercises the real
    /// `uid_for`/`gid_for` + `Command::uid`/`gid` path without needing root
    /// in CI - both ids drop together here, since the current user's own
    /// primary gid is, definitionally, whatever `gid_for` will resolve.
    #[tokio::test]
    async fn running_as_the_current_user_succeeds() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let user = current_username();
        run(
            "web",
            rel.path(),
            &spec,
            Some(&user),
            &[],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("dropping to one's own user and group is always permitted");
    }

    /// fails if a nonexistent `as_user` is silently accepted instead of
    /// refused. The reject side of the same predicate as the test above:
    /// resolving a user that does not exist on this host must fail loudly
    /// rather than, say, running as whatever uid/gid `0` or an unparsed
    /// value would mean.
    ///
    /// Asserts the *gid* wording specifically, not just `Error::Config(_)`
    /// in general: `run` resolves the gid before the uid (see `run`'s own
    /// doc comment for why), so for the same unknown user it is `gid_for`
    /// that fails first. A weaker assertion - `Error::Config(_)` alone -
    /// would keep passing even if the `child.gid(...)` call were deleted
    /// from `run` entirely, since `uid_for` would then fail instead and
    /// still produce *some* `Error::Config`; the specific wording is what
    /// proves the gid step actually ran. `uid_for_reports_a_config_error_
    /// for_an_unknown_user` below covers the uid half of the same shared
    /// `resolve_id` logic directly, independent of which one `run` happens
    /// to call first.
    #[tokio::test]
    async fn an_unknown_as_user_is_a_config_error() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("true".into()),
            ..Default::default()
        };
        let err = run(
            "web",
            rel.path(),
            &spec,
            Some("shep-deploy-test-no-such-user"),
            &[],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a gid"));
    }

    /// fails if `uid_for` stops resolving to a known user's real uid.
    /// Exercised directly rather than through `run`, so it is not
    /// entangled with whichever of `uid_for`/`gid_for` `run` happens to
    /// call first.
    #[tokio::test]
    async fn uid_for_resolves_the_current_users_own_uid() {
        let user = current_username();
        let uid = uid_for(Path::new("."), &user).await.expect("resolves");
        assert_eq!(uid, current_uid());
    }

    /// fails if `gid_for` stops resolving to a known user's *primary* gid.
    /// This is the exact resolution the fix round added: `run` originally
    /// dropped only uid, which left a build's effective gid at whatever the
    /// shepherd's own process gid was - root's, if the shepherd runs as
    /// root specifically to be able to drop privileges at all, which the
    /// design spec recommends.
    #[tokio::test]
    async fn gid_for_resolves_the_current_users_primary_gid() {
        let user = current_username();
        let gid = gid_for(Path::new("."), &user).await.expect("resolves");
        assert_eq!(gid, current_gid());
    }

    /// fails if `uid_for`'s reject side stops naming its own specific
    /// wording. The `uid_for` half of `resolve_id`'s two call sites -
    /// `an_unknown_as_user_is_a_config_error` above only exercises the
    /// `gid_for` half, since that is the one `run` reaches first for the
    /// same bad username.
    #[tokio::test]
    async fn uid_for_reports_a_config_error_for_an_unknown_user() {
        let err = uid_for(Path::new("."), "shep-deploy-test-no-such-user")
            .await
            .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a uid"));
    }

    /// The `gid_for` counterpart of the test above, exercised directly for
    /// the same reason.
    #[tokio::test]
    async fn gid_for_reports_a_config_error_for_an_unknown_user() {
        let err = gid_for(Path::new("."), "shep-deploy-test-no-such-user")
            .await
            .expect_err("no such user");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("does not resolve to a gid"));
    }
}
