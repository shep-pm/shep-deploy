//! Builds of one repository run one after the other, and the lock that does
//! it is let go before the swap.

use std::fs;
use std::time::Duration;

use shep_client::shep_core::config::{AppConfig, DogTable};
use shep_client::shep_core::protocol::ProcessInfo;
use tokio::time::timeout;

use crate::daemon::Daemon;
use crate::deploy::test_support::{
    FIXTURE_TABLE, Fixture, Shepherd, commit_on_origin, fixture_with_previous_release,
};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::fixtures;
use crate::paths::{Repo, Tree};
use crate::state::State;
use crate::{git, swap};

/// Each deploy, its build, its hooks and its verification window, inside this.
/// Longer than all of them and far short of a hang, which is what makes the
/// two awaits below a bound the test sets rather than a hope.
const WINDOW: Duration = Duration::from_secs(90);

/// How long a build holds the turn it has, and how long the release hook holds
/// it on. A build that ran alongside another would land its line inside the
/// other's pair rather than after it.
const TURN: &str = "sleep 1";

/// The fixture's clone, moved under `repos` where a shared one lives, with the
/// fixture tree's `git` linked to it.
fn shared_clone(fixture: &Fixture) -> Repo {
    let repo = Repo::for_remote(fixture.home.path(), &fixture.state.remote);
    fs::create_dir_all(repo.root()).expect("repo dir");
    fs::rename(fixture.tree.git(), repo.git()).expect("move the clone");
    fixture.tree.link_clone(&repo).expect("link the tree's git");
    repo
}

/// A second sheep of one remote: its own tree, the same clone, and its first
/// release live under `current`, as `init` leaves a target it has just cut
/// over.
fn second_sheep(fixture: &Fixture, repo: &Repo, name: &str) -> (Tree, State) {
    let tree = Tree::for_sheep(fixture.home.path(), name);
    fs::create_dir_all(tree.root()).expect("the second tree's directory");
    tree.link_clone(repo).expect("link");
    let first = git::remote_head(&repo.git(), "main").expect("head");
    git::worktree_add(&repo.git(), &tree.release(&first), &first).expect("worktree");
    swap::point_at(&tree.current(), &tree.release(&first)).expect("current");
    let state = State {
        remote: fixture.state.remote.clone(),
        checkout: fixture.origin.path().to_owned(),
        deployed: Some(first),
        ..fixtures::state()
    };
    state.write(&tree.state_file()).expect("deploy.toml");
    (tree, state)
}

/// What `name` writes as it builds, as its release hook runs, and after it
/// lands, into one log every sheep of the repository shares.
fn marks(name: &str, log: &std::path::Path) -> String {
    let log = log.display();
    format!(
        "command = 'echo {name} >> {log} && {TURN}'\n\
         [release]\ncommand = 'echo {name}-hook >> {log} && {TURN}'\n\
         [after]\ncommand = 'echo {name}-landed >> {log}'\n"
    )
}

/// A shepherd holding both sheep, each with its own `[app.dogs.deploy]` table,
/// and reloading whichever it is asked about.
struct TwoSheep {
    web: Shepherd,
    worker: Shepherd,
}

impl TwoSheep {
    fn holding(web: &str, worker: &str) -> Self {
        Self {
            web: Shepherd::ready().with_settings(web),
            worker: Shepherd::ready().with_app(fixtures::gated_app(
                "worker",
                &format!("{FIXTURE_TABLE}{worker}"),
            )),
        }
    }

    fn one(&self, sheep: &str) -> &Shepherd {
        if sheep == "worker" {
            &self.worker
        } else {
            &self.web
        }
    }
}

impl Daemon for TwoSheep {
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

/// fails if two sheep of one remote build at the same time, or if the one that
/// built first holds the build turn past its own swap.
///
/// The two deploys run together, against one clone, for one head, each with
/// its own `command`. Every one of the six lines they write lands in one log,
/// so the log is the record of what ran when: a build that went ahead while
/// another held the turn would put its line inside the other's build-and-hook
/// pair, and a turn held past the swap would put the first sheep's `-landed`
/// line before the second sheep ever built.
///
/// Not paused, for the reason on
/// [`a_current_that_moved_during_the_build_is_not_swapped_over`]: the build
/// commands are real subprocesses, and a paused clock runs the timeout out
/// while the runtime waits for one.
#[tokio::test]
async fn two_sheep_of_one_remote_build_one_after_the_other() {
    let mut fixture = fixture_with_previous_release();
    let repo = shared_clone(&fixture);
    let log = fixture.home.path().join("turns.log");
    let (worker_tree, mut worker_state) = second_sheep(&fixture, &repo, "worker");
    let head = commit_on_origin(&fixture, "second.txt");
    let daemon = TwoSheep::holding(&marks("web", &log), &marks("worker", &log));

    let config = fixtures::dog_config();
    let (web, worker) = tokio::join!(
        timeout(
            WINDOW,
            deploy(&daemon, &fixture.tree, &mut fixture.state, &config)
        ),
        timeout(
            WINDOW,
            deploy(&daemon, &worker_tree, &mut worker_state, &config)
        ),
    );

    let web = web
        .expect("the deploy finished inside the test's own window")
        .expect("deployed");
    let worker = worker.expect("and the other did too").expect("deployed");
    assert_eq!(web, Outcome::Deployed { sha: head.clone() }, "{web:?}");
    assert_eq!(worker, Outcome::Deployed { sha: head }, "{worker:?}");
    assert_eq!(daemon.web.reload_count(), 1, "web was reloaded once");
    assert_eq!(daemon.worker.reload_count(), 1, "worker was reloaded once");

    let shown: Vec<String> = fs::read_to_string(&log)
        .expect("both builds were logged")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        shown.len(),
        6,
        "two builds, two hooks, two landings: {shown:?}"
    );
    for pair in shown[..4].chunks(2) {
        assert_eq!(
            pair[0].trim_end_matches("-hook"),
            pair[1].trim_end_matches("-hook"),
            "one build of one repository at a time, with its release hook \
             inside its own turn: {shown:?}"
        );
    }
    assert!(
        shown[4].ends_with("-landed") && shown[5].ends_with("-landed"),
        "both swaps happen after both builds, so neither holds the turn past \
         its own swap: {shown:?}"
    );
}
