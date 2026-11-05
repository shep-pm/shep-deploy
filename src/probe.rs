//! What this dog declares to shep, answered before it does anything else.
//!
//! Three flags, and one declaration that is read by all three:
//!
//! - `--version`: the crate's version and the protocol number this binary was
//!   compiled against. `shep restart deploy` asks it of the binary on disk to
//!   see whether the dog has been upgraded under the running process.
//! - `--schema`: the JSON Schema of [`crate::settings::Settings`], which is what
//!   `shep lookout` draws its settings form over.
//! - the manifest flag: the name, the version, that schema, which sheep the
//!   settings apply to, and what this dog asks of the shepherd.
//!
//! The declaration is here rather than spread across `main` because the three
//! answers are one statement about one thing, and because the grants are the
//! part an operator has to read twice.
//!
//! # The grants
//!
//! Since shep 0.21 the shepherd refuses a dog any request outside what its
//! manifest asked for, and a dog that asks for nothing is adopted with
//! `grants: []` and then refused its first `sheep_config`, with the refusal
//! only in the dog's own log. So each request [`crate::daemon`] sends is
//! covered by a permission above: reads for `ListFlock`, `Describe` and
//! `SheepConfig`, control for `Start`, `Reload`, `Restart` and `SaveRoll`, and
//! edit for `Delete` and `SetSmit`. A dog's own section (`DogConfigResolved`,
//! `SetSheepDogSettings`) needs no grant.
//!
//! `SheepDogSettingsResolved`, the read of a sheep's table that
//! [`crate::settings::held`] goes through, is the one request here that takes a
//! scoped permission: `secrets.read` in the environment that sheep resolves
//! in, which is the shepherd's `[daemon] environment` unless the sheep names
//! its own. `production` is that default, so this covers an ordinary flock; a
//! flock or a sheep set to another environment needs the matching scope
//! granted by hand under `[daemon.dog_grants.deploy]` in `shep.toml`, and the
//! refusal names the scope. `shep dogs grant` cannot add it, because that verb
//! only grants what the dog declared.
//!
//! Without any grant the poll loop is refused on every target of every tick
//! and fetches nothing. That is what broke the integration tier on 2026-11-03
//! with every local gate green: the gate is only on a dog's own connection, so
//! a one-shot verb is refused nothing and a fake `Daemon` has no gate at all.
//! `crate::poll`'s supervised tests are the only ones that can see it, and
//! `poll::a_supervised_dog_runs_a_sheeps_hook_with_its_secret_resolved` is the
//! one that fails fast.

use shep_client::dogs::{Permission, Probe, Serves, Subcommand, Verb};

use crate::settings;

/// How `shep deploy` is described in `shep --help`, its help page and completion.
fn verb() -> Verb {
    let mut verb = Verb::new("Deploy a git branch and roll back if it does not come up")
        .with_examples([
            "shep deploy api",
            "shep deploy init api --verify alive",
            "shep deploy survey",
        ]);
    verb.subcommands = vec![
        Subcommand::new("deploy", "deploy one sheep, or set how it is watched")
            .with_usage("<sheep> [--watch auto|manual] [--branch <name>] [--stage] [--promote]"),
        Subcommand::new("init", "take a sheep over").with_usage("<sheep> [--verify probed|alive]"),
        Subcommand::new("survey", "report where every registered sheep stands"),
    ];
    verb
}

/// Answers shep's probes, and returns on every run that is not one of them.
///
/// Called as the first line of `main`, before the runtime is built and before
/// anything is opened, because that order is the whole point. A dog that does
/// not recognise `--version` ignores it and starts doing its ordinary job
/// instead - here, a poll loop against the live shepherd - and shep kills that
/// process group a second or two later. This dog builds and cuts over releases,
/// so "runs briefly and is then killed" is not a harmless overlap.
///
/// [`mod@crate::route`] never sees either flag as a result, which is also why
/// neither needs an arm there: a leading `-` is not a sheep name, so both used
/// to land on `USAGE` and exit 2, and shep read that as a dog with no protocol
/// and no schema.
///
/// The name and the version are passed rather than read inside shep-client
/// because `env!` expands where it is written, and there it would report
/// shep-client's own version.
///
/// # Exits
///
/// Ends the process with status 0 on a probe run, before `main` opens anything.
pub(crate) fn declare() {
    Probe::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        .verb(verb())
        .permission(Permission::new(
            "flock.read",
            true,
            "reads each sheep's registered definition and status",
        ))
        .permission(Permission::new(
            "flock.control",
            true,
            "starts, reloads and restarts sheep, and saves the roll",
        ))
        .permission(Permission::new(
            "flock.edit",
            true,
            "deletes the sheep a cutover replaces and sets their smit",
        ))
        .permission(Permission::new(
            "secrets.read:production",
            true,
            "reads each sheep's `[app.dogs.deploy]` table with its `{{secret:...}}` \
             references resolved, so a hook's `env` carries the credential rather than \
             the text of a reference to it",
        ))
        // The dog's `[deploy]` section is the value for every sheep, but a
        // sheep only takes part once it carries a table of its own, which is
        // what `init` gives it. `sheep_settings` reads the same value back.
        .serves(Serves::OptedIn)
        .answer::<settings::Settings>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verb_fits_shep_help_and_names_each_operator_verb() {
        let verb = verb();
        assert!(verb.description.chars().count() <= 60);
        let names: Vec<_> = verb.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "init", "survey"]);
    }
}
