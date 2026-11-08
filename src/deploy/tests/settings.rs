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
    let release = fixture.release_of(&daemon, &second);
    assert!(release.join("from-settings").is_file(), "the sheep's build");
    assert!(
        !release.join("from-flockfile").exists(),
        "the release's build"
    );
}

/// fails if a release's own Flockfile is still read for the app: one that
/// changes `script` and sets a `user` would then either be refused or choose
/// who the build runs as. The sheep's stored config is the app, so the deploy
/// goes through. The app a deploy stages is pinned in
/// `deploy::prepare`'s tests.
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

    let outcome = deploy(
        &Shepherd::ready(),
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect("completes");

    assert_eq!(outcome, Outcome::Deployed { sha: second });
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
        Some(fixture.release(&previous))
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
    let release = fixture.release(&second);
    for ran in ["built", "release-ran", "after-ran"] {
        assert!(!release.join(ran).exists(), "{ran}");
    }
}

/// fails if a build gets a `{{secret:...}}` reference in `build_env` instead
/// of the secret's value, or no `build_env` at all.
///
/// Through a whole deploy, because `deploy` reads its settings through
/// [`crate::settings::held`], the request that resolves the table's references:
/// a dog that took the table out of the stored config would build with
/// `{{secret:SUPABASE_ANON}}` as the key.
///
/// Not paused, for the reason on `a_current_that_moved_during_the_build_is_not_swapped_over`.
#[tokio::test]
async fn a_build_env_secret_reaches_the_build_resolved() {
    let mut fixture = fixture_with_previous_release();
    let second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready()
        .with_settings(
            "command = 'printenv VITE_KEY > key.txt'\n\
             build_env = { VITE_KEY = '{{secret:SUPABASE_ANON}}' }\n",
        )
        .resolving(&[("SUPABASE_ANON", "anon-key-value")]);

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
    let seen = fs::read_to_string(fixture.release_of(&daemon, &second).join("key.txt"))
        .expect("the build ran");
    assert_eq!(seen.trim(), "anon-key-value");
}

/// fails if a `build_env` reference to a secret the sheep's environment lacks
/// still builds, or if the refusal quotes a value.
///
/// The refusal is the shepherd's, as in a real run: it cannot resolve the key,
/// so the table request fails and the deploy never reaches the build. The
/// secret that does exist is there so the test can see that no value, the
/// resolved one or the reference's neighbour, turns up in the message.
#[tokio::test]
async fn a_build_env_secret_that_does_not_exist_refuses_the_deploy_naming_the_key() {
    let mut fixture = fixture_with_previous_release();
    let _second = commit_on_origin(&fixture, "second.txt");
    let daemon = Shepherd::ready()
        .with_settings(
            "command = 'touch built'\n\
             build_env = { PROD_DB_URL = '{{secret:MISSING_DB}}', \
             OTHER = '{{secret:PRESENT}}' }\n",
        )
        .resolving(&[("PRESENT", "hunter2")]);

    let err = deploy(
        &daemon,
        &fixture.tree,
        &mut fixture.state,
        &fixtures::dog_config(),
    )
    .await
    .expect_err("refused");

    let shown = err.to_string();
    assert!(shown.contains("MISSING_DB"), "{shown}");
    assert!(!shown.contains("hunter2"), "{shown}");
}
