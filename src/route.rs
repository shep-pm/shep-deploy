//! Routing: what an argv means to do, decided without doing it.
//!
//! Split out of `main` so the decision - which pattern wins when several
//! could match - is testable on its own, and so the entry point stays small
//! as verbs are added. `main` answers `--version` and `--schema` before it
//! gets here; see the note on its first line.

use crate::paths;

/// What a direct invocation accepts. Printed on anything else.
pub const USAGE: &str = "\
usage: shep-deploy <verb> [args]

  deploy <sheep> [--watch auto|manual]   deploy one sheep, or set how it is watched
  setup <sheep>                          take a sheep over
  survey                                 report where every registered sheep stands
  on-remove                              lifecycle hook; shep runs this itself

Adopted as `deploy`, the same verbs run as `shep deploy <verb> [args]`, and
`shep deploy <sheep>` deploys one sheep directly. A sheep whose name is one of
the verbs above is reached with the verb spelled out: `shep deploy deploy survey`.";

/// What a parsed argv means to do.
///
/// Split out of `main` so the routing decision - which pattern wins when
/// several could match - is testable on its own. A match that both decides
/// and acts can only be exercised by actually running the binary.
#[derive(Debug, PartialEq, Eq)]
pub enum Route<'a> {
    /// No argv at all: the supervised poll loop, per the dog contract.
    Poll,
    /// shep's on-remove lifecycle hook: put every sheep back.
    OnRemove,
    /// Report where every registered sheep stands.
    Survey,
    /// Deploy the named sheep.
    Deploy(&'a str),
    /// Take the named sheep over, up to and including the cutover.
    Setup(&'a str),
    /// Set the named sheep's watch mode.
    Watch { sheep: &'a str, mode: &'a str },
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
pub fn route<'a>(args: &[&'a str]) -> Route<'a> {
    let route = match args {
        [] => Route::Poll,
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
        ["deploy", sheep] => Route::Deploy(sheep),
        ["deploy", sheep, "--watch", mode] => Route::Watch { sheep, mode },
        // Reached only through `shep deploy <sheep>`: the passthrough
        // shipped in shep 0.1.1 and strips the dog's own name, so the
        // flagship command arrives as a bare sheep name with no verb.
        // Last, so a verb always wins.
        [sheep] => Route::Deploy(sheep),
        [sheep, "--watch", mode] => Route::Watch { sheep, mode },
        _ => Route::Usage,
    };

    // Every name that reaches a `Tree` comes through here, so this is the one
    // place it has to be a name rather than a path. `Tree::for_sheep` joins it
    // onto `$SHEP_HOME/deploy`, and an absolute name replaces that root
    // outright rather than traversing out of it.
    match route {
        Route::Setup(sheep) | Route::Deploy(sheep) if !paths::is_sheep_name(sheep) => Route::Usage,
        Route::Watch { sheep, .. } if !paths::is_sheep_name(sheep) => Route::Usage,
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
        assert_eq!(route(&["koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["deploy", "koji"]), Route::Deploy("koji"));
        assert_eq!(route(&["survey"]), Route::Survey);
        assert_eq!(route(&["setup", "koji"]), Route::Setup("koji"));
        // The escape hatch for a sheep whose name is a verb.
        assert_eq!(route(&["deploy", "survey"]), Route::Deploy("survey"));
        assert_eq!(route(&[]), Route::Poll);
    }

    /// fails if `on-remove` stops routing to its own hook, or starts being
    /// swallowed by the bare-name catch-all - a sheep really could be named
    /// `on-remove`, and it gets the same escape hatch every other verb
    /// does.
    #[test]
    fn on_remove_routes_to_its_own_hook() {
        assert_eq!(route(&["on-remove"]), Route::OnRemove);
        assert_eq!(route(&["deploy", "on-remove"]), Route::Deploy("on-remove"));
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
        assert_eq!(route(&["setup"]), Route::Usage);
        assert_eq!(route(&["deploy"]), Route::Usage);

        // And the escape hatch still reaches a sheep really called that, the
        // same one a sheep named `survey` or `on-remove` uses.
        assert_eq!(route(&["deploy", "setup"]), Route::Deploy("setup"));
        assert_eq!(route(&["deploy", "deploy"]), Route::Deploy("deploy"));
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
            assert_eq!(route(&[name]), Route::Usage, "bare: {name}");
            assert_eq!(route(&["deploy", name]), Route::Usage, "deploy: {name}");
            assert_eq!(route(&["setup", name]), Route::Usage, "setup: {name}");
            assert_eq!(
                route(&[name, "--watch", "auto"]),
                Route::Usage,
                "watch: {name}"
            );
        }
    }
}
