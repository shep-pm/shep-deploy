//! A build belongs to a repository, not to a sheep: every sheep of one remote
//! whose `command`, `user`, `checkout` and `build_env` match lands the same
//! built release, and a sheep whose four do not match gets a release of its own.
//! These four are the reuse rule. Separate checkouts are separated in
//! `crate::optin::shared_clone_tests`; what is here is the sharing, the
//! rollback that must not reach across it, and the marker that authorises
//! skipping a build.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use shep_client::shep_core::config::{AppConfig, DogTable};
use shep_client::shep_core::protocol::ProcessInfo;

use crate::daemon::Daemon;
use crate::deploy::test_support::{
    FIXTURE_TABLE, Fixture, Shepherd, another_sheep, commit_on_origin,
    fixture_with_previous_release, shared_clone,
};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::fixtures;
use crate::paths::Tree;
use crate::swap;

/// A shepherd holding every sheep of one repository, each as its own app with
/// its own `[app.dogs.deploy]` table.
struct Flock {
    held: BTreeMap<String, Shepherd>,
}

impl Flock {
    fn holding<'a>(sheep: impl IntoIterator<Item = (&'a str, Shepherd)>) -> Self {
        Self {
            held: sheep
                .into_iter()
                .map(|(name, daemon)| (name.to_owned(), daemon))
                .collect(),
        }
    }

    fn one(&self, sheep: &str) -> &Shepherd {
        self.held.get(sheep).expect("the flock holds this sheep")
    }
}

impl Daemon for Flock {
    async fn sheep_config(&self, sheep: &str) -> Result<AppConfig, Error> {
        self.one(sheep).sheep_config(sheep).await
    }

    async fn sheep_dog_table(&self, sheep: &str, dog: &str) -> Result<Option<DogTable>, Error> {
        self.one(sheep).sheep_dog_table(sheep, dog).await
    }

    async fn describe(&self, sheep: &str) -> Result<Vec<ProcessInfo>, Error> {
        self.one(sheep).describe(sheep).await
    }

    async fn reload(&self, sheep: &str) -> Result<(), Error> {
        self.one(sheep).reload(sheep).await
    }

    crate::fixtures::daemon_methods!(unimplemented;
        dog_config, list_flock, start, delete, restart, save_roll, set_sheep_dog_settings, set_smit,
    );
}

/// The shepherd of `name`, building with `settings` and dropping to the user
/// running the tests.
///
/// `user` is set because it is one of the three inputs of the build key, and
/// the only user a dog may drop to without privilege is the one it already is.
fn a_sheep(name: &str, settings: &str) -> Shepherd {
    Shepherd::ready()
        .with_app(fixtures::gated_app(
            name,
            &format!("{FIXTURE_TABLE}{settings}"),
        ))
        .with_user(&fixtures::current_user())
}

/// The directory `name`'s build of `head` lands in, and the file it writes to
/// prove which build it was.
fn prove(
    fixture: &Fixture,
    daemon: &Flock,
    name: &str,
    head: &str,
    marker: &str,
) -> (PathBuf, PathBuf) {
    let release = fixture.release_of(daemon.one(name), head);
    (release.clone(), release.join(marker))
}

/// fails if every sheep of one remote pays for the same build.
///
/// Three sheep, one remote, one head, one `command`, one `user`, no
/// `build_env`: all three build keys are the same, so the release the first
/// one checks out and builds is the release the other two land. The log is the
/// count, and the three `current`s are the sharing: every one of them names one
/// directory.
///
/// Not paused, for the reason on
/// `crate::deploy::tests::build_lock::two_sheep_of_one_remote_build_one_after_the_other`:
/// the build command is a real subprocess.
#[tokio::test]
async fn three_sheep_of_one_remote_build_one_sha_once() {
    let mut fixture = fixture_with_previous_release();
    let repo = shared_clone(&fixture);
    let log = fixture.home.path().join("builds.log");
    let head = commit_on_origin(&fixture, "second.txt");
    let settings = format!("command = 'echo built >> {}'\n", log.display());
    let daemon = Flock::holding(
        ["web", "api", "worker"]
            .into_iter()
            .map(|name| (name, a_sheep(name, &settings))),
    );
    let (api_tree, mut api_state) = another_sheep(&fixture, &repo, "api", daemon.one("api"));
    let (worker_tree, mut worker_state) =
        another_sheep(&fixture, &repo, "worker", daemon.one("worker"));

    let config = fixtures::dog_config();
    let (web, api, worker) = tokio::join!(
        deploy(&daemon, &fixture.tree, &mut fixture.state, &config),
        deploy(&daemon, &api_tree, &mut api_state, &config),
        deploy(&daemon, &worker_tree, &mut worker_state, &config),
    );

    for outcome in [web, api, worker] {
        assert_eq!(
            outcome.expect("the deploy finished"),
            Outcome::Deployed { sha: head.clone() }
        );
    }

    let built = fs::read_to_string(&log).expect("the build log");
    assert_eq!(
        built.lines().count(),
        1,
        "one build for three sheep of one remote: {built:?}"
    );

    let shared = fixture.release_of(daemon.one("web"), &head);
    for tree in [&fixture.tree, &api_tree, &worker_tree] {
        assert_eq!(
            swap::resolve(&tree.current()).expect("current"),
            Some(shared.clone()),
            "{} serves the release the repository built once",
            tree.sheep()
        );
    }
}

/// fails if two sheep of one remote that build one sha with different
/// `build_env` land on one release: one of them would then serve a build made
/// with the other's environment.
///
/// The two deploys run one after the other, so the second has a finished
/// release of that sha sitting there and reusing it is the tempting wrong
/// answer. Each build writes the environment value it was given, which is what
/// separates "its own release" from "the other one's".
///
/// Not paused, for the reason on [`three_sheep_of_one_remote_build_one_sha_once`].
#[tokio::test]
async fn a_different_build_env_builds_a_release_of_its_own() {
    let mut fixture = fixture_with_previous_release();
    let repo = shared_clone(&fixture);
    let head = commit_on_origin(&fixture, "second.txt");
    let building = |flavour: &str| {
        format!(
            "command = 'printenv FLAVOUR > flavour.txt'\n\
             build_env = {{ FLAVOUR = {flavour:?} }}\n"
        )
    };
    let daemon = Flock::holding([
        ("web", a_sheep("web", &building("web"))),
        ("api", a_sheep("api", &building("api"))),
    ]);
    let (api_tree, mut api_state) = another_sheep(&fixture, &repo, "api", daemon.one("api"));

    let config = fixtures::dog_config();
    let web = deploy(&daemon, &fixture.tree, &mut fixture.state, &config)
        .await
        .expect("web deploys");
    let api = deploy(&daemon, &api_tree, &mut api_state, &config)
        .await
        .expect("api deploys");
    assert_eq!(web, Outcome::Deployed { sha: head.clone() }, "{web:?}");
    assert_eq!(api, Outcome::Deployed { sha: head.clone() }, "{api:?}");

    let (web_release, web_flavour) = prove(&fixture, &daemon, "web", &head, "flavour.txt");
    let (api_release, api_flavour) = prove(&fixture, &daemon, "api", &head, "flavour.txt");
    assert_ne!(
        web_release, api_release,
        "one sha, two build environments, two releases"
    );
    for (release, marker, flavour) in [
        (&web_release, &web_flavour, "web"),
        (&api_release, &api_flavour, "api"),
    ] {
        assert_eq!(
            fs::read_to_string(marker).expect("the build ran").trim(),
            flavour,
            "{release:?} was built with FLAVOUR = {flavour}"
        );
    }
    assert_eq!(
        swap::resolve(&api_tree.current()).expect("api's current"),
        Some(api_release),
        "and api is serving the one its own build wrote"
    );
}

/// fails if rolling one sheep back moves another sheep's `current`.
///
/// Two sheep of one remote with the same three build inputs, so they share both
/// releases: the head they are deploying and the one they both started from.
/// The head comes up for `api` and never comes up for `web`, and `web`'s
/// rollback repoints its own `current` at the release it came back to. `api` is
/// serving out of the very release `web` abandoned, and must still be.
#[tokio::test(start_paused = true)]
async fn a_rolled_back_sheep_leaves_the_other_sheeps_current_alone() {
    let mut fixture = fixture_with_previous_release();
    let first = fixture.state.deployed.clone().expect("a previous release");
    let repo = shared_clone(&fixture);
    let head = commit_on_origin(&fixture, "second.txt");
    let daemon = Flock::holding([("web", Shepherd::never_ready()), ("api", Shepherd::ready())]);
    let (api_tree, mut api_state) = another_sheep(&fixture, &repo, "api", daemon.one("api"));

    let config = fixtures::dog_config();
    let web = deploy(&daemon, &fixture.tree, &mut fixture.state, &config)
        .await
        .expect("completes");
    assert!(matches!(web, Outcome::RolledBack { .. }), "{web:?}");

    let api = deploy(&daemon, &api_tree, &mut api_state, &config)
        .await
        .expect("api deploys the same head");
    assert_eq!(api, Outcome::Deployed { sha: head.clone() }, "{api:?}");

    let tree: &Tree = &api_tree;
    let built = fixture.release_of(daemon.one("api"), &head);
    assert_eq!(
        swap::resolve(&tree.current()).expect("api's current"),
        Some(built),
        "api is still serving the head web rolled back off"
    );
    assert_eq!(
        swap::resolve(&fixture.tree.current()).expect("web's current"),
        Some(fixture.release(&first)),
        "and only web's own current moved"
    );
}

/// fails if a completion marker outlives the release it vouches for, or is
/// inherited by the checkout that replaces it.
///
/// The marker is a separate file in a separate directory, so an operator's own
/// `rm -rf` of a release, or another sheep's prune, leaves it where it was. It
/// is what authorises skipping a build, and a release directory that is there
/// without its marker is replaced by a fresh checkout. If that build then fails,
/// the new directory and the stale marker together read as a finished release to
/// the next attempt, which would skip its build and land whatever the failed one
/// left. So the marker goes before the replacement is checked out, and only a
/// build that just succeeded ever writes one.
///
/// The build command both writes to the log and fails, so the log is the count
/// of builds and the failure is the point: a build that never ran would add no
/// line. Not paused, for the reason on
/// [`three_sheep_of_one_remote_build_one_sha_once`].
#[tokio::test]
async fn a_stale_marker_does_not_vouch_for_a_replacement_checkout() {
    let mut fixture = fixture_with_previous_release();
    let head = commit_on_origin(&fixture, "second.txt");
    let log = fixture.home.path().join("builds.log");
    let daemon = Shepherd::ready().with_settings(&format!(
        "command = 'echo built >> {}; exit 3'\n",
        log.display()
    ));

    fs::create_dir_all(fixture.completions()).expect("the markers directory");
    fs::write(fixture.completion_of(&daemon, &head), b"")
        .expect("a marker with no release behind it");

    for attempt in 1..=2 {
        let err = deploy(
            &daemon,
            &fixture.tree,
            &mut fixture.state,
            &fixtures::dog_config(),
        )
        .await
        .expect_err("the build fails on both attempts");
        assert!(
            matches!(err, Error::Build { status: Some(3) }),
            "{attempt}: {err:?}"
        );
        assert!(
            !fixture.completion_of(&daemon, &head).is_file(),
            "attempt {attempt} left a marker vouching for a release whose build failed"
        );
    }

    let built = fs::read_to_string(&log).expect("the build log");
    assert_eq!(
        built.lines().count(),
        2,
        "the second attempt rebuilt, rather than reusing the checkout the \
         first one failed to build: {built:?}"
    );
}
