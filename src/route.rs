//! Routing: what an argv means to do, decided without doing it.
//!
//! Split out of `main` so the decision - which pattern wins when several
//! could match - is testable on its own, and so the entry point stays small
//! as verbs are added. `main` answers `--version` and `--schema` before it
//! gets here; see the note on its first line.

use crate::paths;

/// What a direct invocation accepts. Printed on anything else, and on `help`.
pub const USAGE: &str = "\
usage: shep-deploy <verb> [args]

  deploy <sheep> [--watch auto|manual]   deploy one sheep, or set how it is watched
  deploy <sheep> --branch <name>         follow another branch; returns without deploying
  deploy <sheep> --stage                 build the branch head and record it; no swap
  setup <sheep> [--verify probed|alive]  take a sheep over; alive if it has nothing to probe
  survey                                 report where every registered sheep stands
  on-remove                              lifecycle hook; shep runs this itself
  help                                   print this

With no arguments the poll loop starts only when SHEP_DOG_NAME is set, as shep
does when it starts the dog; without it, the bare command prints this and exits.

Adopted as `deploy`, the same verbs run as `shep deploy <verb> [args]`, and
`shep deploy <sheep>` deploys one sheep directly. A sheep whose name is one of
the verbs above is reached with the verb spelled out: `shep deploy deploy survey`,
`shep deploy deploy help`.";

/// What a parsed argv means to do.
///
/// Split out of `main` so the routing decision - which pattern wins when
/// several could match - is testable on its own. A match that both decides
/// and acts can only be exercised by actually running the binary.
#[derive(Debug, PartialEq, Eq)]
pub enum Route<'a> {
    /// No argv, and shep started this process: the poll loop, per the dog
    /// contract.
    Poll,
    /// `-h`, `--help` or `help`: print [`USAGE`] on stdout and exit 0.
    Help,
    /// shep's on-remove lifecycle hook: put every sheep back.
    OnRemove,
    /// Report where every registered sheep stands.
    Survey,
    /// Deploy the named sheep.
    Deploy(&'a str),
    /// Take the named sheep over, up to and including the cutover.
    Setup(&'a str),
    /// Take the named sheep over, recording `mode` as its verification mode.
    SetupVerify { sheep: &'a str, mode: &'a str },
    /// Set the named sheep's watch mode.
    Watch { sheep: &'a str, mode: &'a str },
    /// Build the named sheep's branch head and record it, without swapping.
    Stage(&'a str),
    /// Point the named sheep at another branch.
    Branch { sheep: &'a str, name: &'a str },
    /// Nothing above matched; print [`USAGE`] and exit on the usage code.
    Usage,
}

/// Decides what an argv means, without acting on it.
///
/// Verb forms are matched before the passthrough arms, deliberately: shep's
/// dog passthrough strips this dog's own name, so `shep deploy koji` and
/// `shep-deploy koji` both arrive as `["koji"]`, indistinguishable from a
/// sheep whose name really is a verb. Checking verbs first means a sheep
/// named `survey` needs the explicit form, `shep deploy deploy survey`,
/// which is the escape hatch [`USAGE`] documents rather than a silent trap.
/// `help` is one of those verbs.
///
/// `supervised` is whether `SHEP_DOG_NAME` is set. Only then does an empty argv
/// mean the poll loop; a person who typed the bare command gets usage. shep sets
/// the variable when it starts the dog and also when a person types a bare
/// `shep deploy`, and the two cannot be told apart from inside this process.
pub fn route<'a>(args: &[&'a str], supervised: bool) -> Route<'a> {
    let route = match args {
        [] if supervised => Route::Poll,
        [] => Route::Usage,
        ["-h" | "--help" | "help"] => Route::Help,
        ["on-remove"] => Route::OnRemove,
        ["survey"] => Route::Survey,
        // A verb whose sheep is missing, before the bare-name arm below can
        // read the verb itself as a sheep. `shep-deploy setup` used to mean
        // "deploy the sheep called setup", which is a different command
        // against a sheep that almost never exists.
        //
        // A sheep genuinely named `setup` or `deploy` is reached the same way
        // one named `survey` already is, by spelling the verb out:
        // `shep deploy setup`. That is the escape hatch USAGE documents.
        ["setup" | "deploy"] => Route::Usage,
        ["setup", sheep] => Route::Setup(sheep),
        ["setup", sheep, "--verify", mode] => Route::SetupVerify { sheep, mode },
        ["deploy", sheep] => Route::Deploy(sheep),
        ["deploy", sheep, "--watch", mode] => Route::Watch { sheep, mode },
        ["deploy", sheep, "--branch", name] => Route::Branch { sheep, name },
        ["deploy", sheep, "--stage"] => Route::Stage(sheep),
        // Reached only through `shep deploy <sheep>`: the passthrough
        // shipped in shep 0.1.1 and strips the dog's own name, so the
        // flagship command arrives as a bare sheep name with no verb.
        // Last, so a verb always wins.
        [sheep] => Route::Deploy(sheep),
        [sheep, "--watch", mode] => Route::Watch { sheep, mode },
        [sheep, "--branch", name] => Route::Branch { sheep, name },
        [sheep, "--stage"] => Route::Stage(sheep),
        _ => Route::Usage,
    };

    // Every name that reaches a `Tree` comes through here, so this is the one
    // place it has to be a name rather than a path. `Tree::for_sheep` joins it
    // onto `$SHEP_HOME/deploy`, and an absolute name replaces that root
    // outright rather than traversing out of it.
    match route {
        Route::Setup(sheep) | Route::Deploy(sheep) | Route::Stage(sheep)
            if !paths::is_sheep_name(sheep) =>
        {
            Route::Usage
        }
        Route::SetupVerify { sheep, .. }
        | Route::Watch { sheep, .. }
        | Route::Branch { sheep, .. }
            if !paths::is_sheep_name(sheep) =>
        {
            Route::Usage
        }
        named => named,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a bare sheep name stops routing to a deploy, or if it
    /// starts shadowing a verb. `shep deploy koji` is the flagship command
    /// and arrives here as `["koji"]`, with no verb, because the
    /// passthrough strips the dog's own name.
    #[test]
    fn a_bare_name_is_a_deploy_and_a_verb_still_wins() {
        assert_eq!(route(&["koji"], false), Route::Deploy("koji"));
        assert_eq!(route(&["deploy", "koji"], false), Route::Deploy("koji"));
        assert_eq!(route(&["survey"], false), Route::Survey);
        assert_eq!(route(&["setup", "koji"], false), Route::Setup("koji"));
        // The escape hatch for a sheep whose name is a verb.
        assert_eq!(route(&["deploy", "survey"], false), Route::Deploy("survey"));
        assert_eq!(route(&[], true), Route::Poll);
    }

    /// fails if a bare run by hand starts the poll loop again, or if shep
    /// starting the dog stops doing so. The loop is the dog contract; a person
    /// typing the bare command wants usage, and a second loop beside the
    /// adopted one is a deploy racing itself.
    #[test]
    fn a_bare_run_polls_only_when_shep_started_it() {
        assert_eq!(route(&[], true), Route::Poll);
        assert_eq!(route(&[], false), Route::Usage);
    }

    /// fails if any spelling of help stops answering help, or starts depending
    /// on who launched the process.
    #[test]
    fn help_in_each_spelling_routes_to_help() {
        for supervised in [true, false] {
            for arg in ["-h", "--help", "help"] {
                assert_eq!(
                    route(&[arg], supervised),
                    Route::Help,
                    "{arg}, supervised: {supervised}"
                );
            }
        }
    }

    /// fails if a sheep named `help` stops being reachable. It gets the same
    /// escape hatch every other verb does.
    #[test]
    fn a_sheep_named_help_is_reached_with_the_verb_spelled_out() {
        assert_eq!(route(&["deploy", "help"], false), Route::Deploy("help"));
    }

    /// fails if `on-remove` stops routing to its own hook, or starts being
    /// swallowed by the bare-name catch-all - a sheep really could be named
    /// `on-remove`, and it gets the same escape hatch every other verb
    /// does.
    #[test]
    fn on_remove_routes_to_its_own_hook() {
        assert_eq!(route(&["on-remove"], false), Route::OnRemove);
        assert_eq!(
            route(&["deploy", "on-remove"], false),
            Route::Deploy("on-remove")
        );
    }

    /// fails if a verb with its sheep missing is read as a sheep named after
    /// the verb.
    ///
    /// `shep-deploy setup` fell through the two-element arms to the bare-name
    /// catch-all and became `Deploy("setup")`: a different command, aimed at a
    /// sheep that almost certainly does not exist. Usage is what an operator
    /// who forgot the argument needs, and it is what every other verb already
    /// does.
    #[test]
    fn a_verb_without_its_sheep_is_a_usage_error() {
        assert_eq!(route(&["setup"], false), Route::Usage);
        assert_eq!(route(&["deploy"], false), Route::Usage);

        // And the escape hatch still reaches a sheep really called that, the
        // same one a sheep named `survey` or `on-remove` uses.
        assert_eq!(route(&["deploy", "setup"], false), Route::Deploy("setup"));
        assert_eq!(route(&["deploy", "deploy"], false), Route::Deploy("deploy"));
    }

    /// fails if `setup <sheep> --verify <mode>` falls through to a pattern
    /// that reads it as something else. `["setup", sheep, ..]` has no other
    /// arm of its own, so it would reach `Usage`; this pins that the flag has
    /// a variant and that the mode survives to `setup_once` unparsed.
    #[test]
    fn setup_with_verify_routes_to_its_own_variant() {
        assert_eq!(
            route(&["setup", "koji", "--verify", "alive"], false),
            Route::SetupVerify {
                sheep: "koji",
                mode: "alive"
            }
        );
        assert_eq!(
            route(&["setup", "koji", "--verify", "bogus"], false),
            Route::SetupVerify {
                sheep: "koji",
                mode: "bogus"
            },
            "the mode is parsed where it is used, not here"
        );
        assert_eq!(
            route(&["setup", "../koji", "--verify", "alive"], false),
            Route::Usage
        );
    }

    /// fails if a sheep name that is really a path reaches a `Tree`.
    ///
    /// `Tree::for_sheep` joins the name onto `$SHEP_HOME/deploy`, and
    /// `PathBuf::join` REPLACES the path when given an absolute one. So an
    /// absolute name does not traverse out of the tree, it discards the tree
    /// and roots itself wherever it points, taking everything that later
    /// prunes and removes inside it along.
    #[test]
    fn a_sheep_name_that_is_a_path_is_refused() {
        for name in ["/tmp/anywhere", "../sibling", "a/b", "", ".", ".."] {
            assert_eq!(route(&[name], false), Route::Usage, "bare: {name}");
            assert_eq!(
                route(&["deploy", name], false),
                Route::Usage,
                "deploy: {name}"
            );
            assert_eq!(
                route(&["setup", name], false),
                Route::Usage,
                "setup: {name}"
            );
            assert_eq!(
                route(&[name, "--watch", "auto"], false),
                Route::Usage,
                "watch: {name}"
            );
        }
    }

    /// fails if `--branch` stops routing in either spelling, or a verb
    /// missing its name is read as something else. The name is not
    /// validated here: `set_branch` owns that, through `State::validate`.
    #[test]
    fn a_branch_flag_routes_with_and_without_the_verb() {
        let expected = Route::Branch {
            sheep: "koji",
            name: "main",
        };
        assert_eq!(
            route(&["deploy", "koji", "--branch", "main"], false),
            expected
        );
        assert_eq!(route(&["koji", "--branch", "main"], false), expected);
        assert_eq!(route(&["deploy", "koji", "--branch"], false), Route::Usage);
        assert_eq!(
            route(&["deploy", "../koji", "--branch", "main"], false),
            Route::Usage
        );
    }
}
