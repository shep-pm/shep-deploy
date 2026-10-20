//! The environment a build runs in: cleared, then rebuilt from three named
//! sources and nothing else.

use std::collections::BTreeMap;
use std::env;

use tokio::process::Command;

/// The environment variables a build keeps from this process, by name.
///
/// Deliberately tiny. `sh -c` needs `PATH` to find anything; a toolchain
/// manager needs `HOME` to find its own installation; `LANG` and `LC_ALL`
/// keep a build's own output readable, and `TZ` keeps a timestamp in a
/// generated artifact from moving with the machine's default. Everything
/// else an operator wants is opted into by name through `passthrough` in
/// `[deploy]`, so it appears in `dogs.toml` where it can be read rather
/// than being inherited invisibly.
///
/// Notably absent and absent on purpose: `SSH_AUTH_SOCK`. A forwarded agent
/// reaching a build means the build can authenticate as the operator
/// anywhere that agent is trusted. `crate::git::fetch` runs in THIS process
/// and keeps its own environment, so a private repository still clones; it
/// is only the build that loses the socket, and an operator who genuinely
/// needs it during a build can name it in `passthrough`.
pub(super) const BASE_ENV: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TZ"];

/// Replaces `child`'s environment with [`BASE_ENV`], then whatever the
/// operator's `passthrough` names, then `spec_env`, and nothing this process
/// itself inherited beyond those.
pub(super) fn apply(
    child: &mut Command,
    sheep: &str,
    passthrough: &[String],
    spec_env: &BTreeMap<String, String>,
) {
    // Cleared, then rebuilt deliberately. Dropping uid and gid below bounds
    // what the build can TOUCH; it does nothing about what it can READ out of
    // its own environment, because those values are copied into the child
    // before any of it happens. A dog started with a registry token or a
    // forwarded agent socket in its environment would hand both to every
    // build it runs, and a build command is chosen by whoever can land a
    // commit on the tracked branch.
    //
    // What survives is named in three places and nowhere else: BASE_ENV, the
    // operator's `passthrough` list, and the release's own `[dog.deploy.build] env`.
    //
    // `var_os` rather than `var`, because `var` reports a value that is not
    // UTF-8 as absent and a `PATH` with one such component then vanished,
    // leaving the build on `sh`'s built-in default path.
    child.env_clear();
    for key in BASE_ENV {
        if let Some(value) = env::var_os(key) {
            child.env(key, value);
        }
    }
    for key in passthrough {
        match env::var_os(key) {
            Some(value) => {
                child.env(key, value);
            }
            // Said, because `passthrough` is the only door for a registry
            // token and a typo in it used to be a build that silently did
            // not get the token. The config is checked at parse time; whether
            // the variable is SET is only known here.
            None => eprintln!(
                "shep-deploy: {sheep}: passthrough names {key}, which is not set in this dog's \
                 environment, so the build does not get it"
            ),
        }
    }
    child.envs(spec_env);
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TEST_BUILD_BUDGET, build};
    use super::super::{BuildSpec, run};
    use crate::fixtures;
    use std::collections::BTreeMap;

    /// fails if the dog's own environment leaks into a build unasked.
    ///
    /// Dropping uid and gid bounds what a build can TOUCH. It does nothing
    /// about what the build can READ out of its environment, because those
    /// values are copied into the child before the drop happens. A dog
    /// started with a registry token in its environment would hand it to
    /// every build, and the build command is chosen by whoever can land a
    /// commit on the tracked branch.
    ///
    /// `CARGO_PKG_NAME` is the probe because cargo sets it for the test
    /// process itself, so it is genuinely present in this process's
    /// environment and genuinely absent from `BASE_ENV`. Setting one here
    /// instead is not available: `std::env::set_var` is unsafe in edition
    /// 2024 and this crate forbids unsafe outright.
    #[tokio::test]
    async fn the_dogs_own_environment_does_not_reach_a_build() {
        assert!(
            std::env::var("CARGO_PKG_NAME").is_ok(),
            "the probe variable must exist in this process or the test proves nothing"
        );
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv CARGO_PKG_NAME > leaked.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        build(rel.path(), &spec).await.expect("builds");
        let leaked = std::fs::read_to_string(rel.path().join("leaked.txt")).unwrap_or_default();
        assert!(
            leaked.trim().is_empty(),
            "the build saw CARGO_PKG_NAME = {leaked:?}, so the environment was inherited"
        );
    }

    /// fails if `BASE_ENV` stops reaching the build.
    ///
    /// Nothing else pins it: with the `BASE_ENV` loop deleted the suite
    /// stayed green, because `sh` resolves `printenv` through its own
    /// built-in default path when `PATH` is unset. A build with no `PATH`
    /// and no `HOME` would find `cargo` and `npm` missing on a host where
    /// both are installed, which reads as a broken host rather than a
    /// dropped variable.
    #[tokio::test]
    async fn the_base_environment_reaches_the_build() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv PATH > path.txt; printenv HOME > home.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        build(rel.path(), &spec).await.expect("builds");
        for name in ["path.txt", "home.txt"] {
            let value = std::fs::read_to_string(rel.path().join(name)).unwrap_or_default();
            assert!(
                !value.trim().is_empty(),
                "{name} is empty, so a BASE_ENV variable did not reach the build"
            );
        }
    }

    /// fails if `passthrough` stops being the way a build gets a variable.
    ///
    /// The counterpart to the test above: the bound is only usable if there
    /// is a way through it, and that way has to be named in `shep.toml` so
    /// the exposure is readable rather than inherited.
    #[tokio::test]
    async fn a_named_passthrough_variable_reaches_the_build() {
        let rel = fixtures::fixture_release(&[]);
        let spec = BuildSpec {
            command: Some("printenv CARGO_PKG_NAME > passed.txt; true".into()),
            env: BTreeMap::new(),
            artifacts: vec![],
        };
        run(
            "web",
            rel.path(),
            &spec,
            None,
            &["CARGO_PKG_NAME".to_owned()],
            fixtures::tempdir().path(),
            TEST_BUILD_BUDGET,
        )
        .await
        .expect("builds");
        assert_eq!(
            std::fs::read_to_string(rel.path().join("passed.txt"))
                .expect("the build wrote it")
                .trim(),
            "shep-deploy"
        );
    }
}
