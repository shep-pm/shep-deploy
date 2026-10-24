//! Where a deploy reads its build and hooks from: the sheep's
//! `[app.dogs.deploy]` table, and nowhere in the release.

use std::fs;

use crate::deploy::test_support::{
    FLOCKFILE, Shepherd, commit_on_origin, fixture_with_previous_release,
};
use crate::deploy::{Outcome, deploy};
use crate::error::Error;
use crate::{fixtures, swap};

/// fails if a release's own Flockfile can choose the build command: whoever
/// can land a commit would then run what they like as the dog.
#[tokio::test]
async fn the_sheeps_command_runs_and_the_flockfiles_is_ignored() {
    let mut fixture = fixture_with_previous_release();
    fs::write(
        fixture.origin.path().join("Flockfile.toml"),
        format!("{FLOCKFILE}\n[dog.deploy.build]\ncommand = 'touch from-flockfile'\n"),
    )
    .expect("write Flockfile");
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready().with_settings("command = 'touch from-settings'\n");

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: second.clone()
        }
    );
    let release = fixture.tree.release(&second);
    assert!(release.join("from-settings").is_file(), "the sheep's build");
    assert!(
        !release.join("from-flockfile").exists(),
        "the release's build"
    );
}

/// fails if a release's own Flockfile is still read for the app: one that
/// changes `script` and sets a `user` would then either be refused or choose
/// who the build runs as. The sheep's stored config is the app, so the deploy
/// goes through and the shepherd's copy is as it was.
#[tokio::test]
async fn a_flockfile_that_changes_script_and_user_deploys_with_the_stored_app() {
    let mut fixture = fixture_with_previous_release();
    fs::write(
        fixture.origin.path().join("Flockfile.toml"),
        FLOCKFILE.replace(
            "script = './run.sh'\n",
            "script = './elsewhere.sh'\nuser = 'root'\n",
        ),
    )
    .expect("write Flockfile");
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready();
    let stored = daemon.stored();

    let outcome = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(outcome, Outcome::Deployed { sha: second });
    assert_eq!(daemon.stored(), stored);
    assert_eq!(stored.script, "./run.sh");
    assert_eq!(stored.user, None);
}

/// fails if a typo in the sheep's table is skipped over, which would deploy
/// with no build and nothing saying why.
#[tokio::test]
async fn an_unknown_key_refuses_the_deploy_naming_the_key_and_the_sheep() {
    let mut fixture = fixture_with_previous_release();
    let previous = fixture.state.deployed.clone().expect("a previous release");
    commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready().with_settings("commnd = 'touch built'\n");

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("refused");

    assert!(matches!(err, Error::Config(_)), "{err:?}");
    let shown = err.to_string();
    assert!(shown.contains("commnd"), "{shown}");
    assert!(shown.contains(fixture.tree.sheep()), "{shown}");
    assert_eq!(daemon.reload_count(), 0);
    assert_eq!(
        swap::resolve(&fixture.tree.current()).unwrap(),
        Some(fixture.tree.release(&previous))
    );
}

/// fails if a sheep with no `[app.dogs.deploy]` table builds or runs a hook
/// from anything else: the release carries a build table and the operator's
/// override carries a hook, and neither is the sheep's.
#[tokio::test]
async fn no_table_deploys_with_no_build_and_no_hooks() {
    let mut fixture = fixture_with_previous_release();
    fs::write(
        fixture.origin.path().join("Flockfile.toml"),
        format!(
            "{FLOCKFILE}\n[dog.deploy.build]\ncommand = 'touch built'\n\
             [dog.deploy.release]\ncommand = 'touch release-ran'\n\
             [dog.deploy.after]\ncommand = 'touch after-ran'\n"
        ),
    )
    .expect("write Flockfile");
    let second = commit_on_origin(&fixture, "second.txt");

    let outcome = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(
        outcome,
        Outcome::Deployed {
            sha: second.clone()
        }
    );
    let release = fixture.tree.release(&second);
    for ran in ["built", "release-ran", "after-ran"] {
        assert!(!release.join(ran).exists(), "{ran}");
    }
}
