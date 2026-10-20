//! The poll loop: what `watch = "auto"` means.
//!
//! Everything else in this crate runs when an operator types a command.
//! This runs unattended, forever, so two properties matter more here than
//! anywhere else: one target's failure must not stop the others, and
//! whatever a tick prints must be true of what it actually did.
//!
//! # What a tick asks the shepherd for
//!
//! One `set_smit` per target and one read of this dog's own section, every
//! tick, and nothing more until it has something to deploy. Targets are read
//! from the filesystem - one directory per target under
//! `<shep_home>/deploy`, each holding its own record - and a target whose
//! branch has not moved is answered by `git` alone. The smit is republished
//! each tick because the daemon holds it in memory only for as long as this
//! dog's connection lasts; see [`tick`]. Past that, the shepherd is asked
//! only by a deploy that is going ahead.
//!
//! The section is re-read rather than read once at startup, because
//! otherwise an operator who changes `interval` or `retention` is told by
//! `shep lookout` that the dog has been told, and the dog goes on polling
//! on the section it read when it started until somebody restarts it. It is
//! a read rather than a subscription to the `config.dog.<name>` the
//! shepherd already publishes: a subscriber that lags drops events, and
//! `ReconnectingClient::subscribe` hands out a stream belonging to one
//! generation of the connection, which this dog outlives on purpose (see
//! `crate::main::poll_forever`). Both mean a bus alone would silently miss
//! a change, which is the failure this exists to remove, so the poll would
//! have had to stay underneath it anyway.
//!
//! In particular the roll is never read. [`crate::roll::registered`] is how
//! `survey` learns what shep has registered, and it goes through
//! `SaveRoll`, which makes the shepherd re-query every live process AND
//! rewrite `flock.json` whether anything changed or not. Once per `survey`
//! is a fair price; once every thirty seconds forever is a disk write per
//! tick for a fact no tick needs.
//!
//! # Serial, and no mid-deploy abort
//!
//! One tick deploys its targets one at a time, and a tick never begins
//! while the previous one is still running, because this is a plain
//! `loop { tick().await; sleep(interval).await; }`. A push landing during a
//! build is therefore picked up on the NEXT tick rather than aborting the
//! one in flight. The design asks for the abort; the cost of deferring it
//! is one build of latency before a hotfix lands, not a wrong outcome.
//!
//! The upside is that [`crate::deploy`]'s own race guard stays a guard.
//! It refuses when `current` moved while a deploy was preparing, and that
//! refusal is there for a second OPERATOR invocation - it never has to hold
//! the loop off itself.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use tokio::time::sleep;

use crate::config::{DogConfig, Reader};
use crate::daemon::Daemon;
use crate::deploy::{self, Outcome};
use crate::error::Error;
use crate::paths::{self, Tree};
use crate::smit;
use crate::state::{State, Watch};

mod report;

use report::{CatchUnwind, Repeat, Said, panic_message, report, worth_saying};

/// Whether the poll loop should deploy this target on its own.
///
/// The only thing `manual` changes. Releases, shared linking, the atomic
/// swap, probe verification, auto-rollback and retention all apply to a
/// manual target identically; the trigger is the whole of the difference.
/// Two cases it serves: somebody who wants the convenience without a deploy
/// on every commit, and pausing a target during an incident without
/// rehoming the dog, which is the same switch and matters more when
/// something is already going wrong.
const fn due(state: &State) -> bool {
    matches!(state.watch, Watch::Auto)
}

/// One pass over every target, deploying each `auto` one that has moved.
///
/// Answers with one row per target it attempted, in name order, rather than
/// stopping at the first failure. A dog that gave up because one of five
/// remotes was unreachable would stop deploying the other four, and nothing
/// would restart it with a reason anybody could read.
///
/// Through [`deploy::unattended`] rather than [`deploy::deploy`], which is
/// the difference between a loop and an operator: a sha an earlier attempt
/// failed on is left alone until the branch moves, rather than rebuilt and
/// rolled back every interval for as long as nobody pushes.
///
/// Targets are re-read from disk every tick rather than cached, so a target
/// created, switched to `manual`, or moved with `--branch` (the branch is
/// read from the checkout once, at setup) is picked up on the next pass.
///
/// Two failures are reported as rows rather than passed over. A record that
/// cannot be read is a target nothing can be done with, and
/// [`paths::targets`] only lists directories that HAVE one, so it is a
/// broken record rather than an ordinary absence. A deploy directory that
/// cannot be listed at all takes every target with it, and its row is named
/// for the directory because there is no sheep left to name; silence there
/// is indistinguishable from a dog with nothing to do.
async fn tick<D: Daemon>(daemon: &D, shep_home: &Path, config: &DogConfig) -> Tick {
    let found = match paths::targets(shep_home) {
        Ok(found) => found,
        Err(err) => {
            return Tick {
                targets: None,
                results: vec![(shep_home.join("deploy").display().to_string(), Err(err))],
            };
        }
    };

    let mut results = Vec::new();
    let targets = Some(found.named.len() + found.unnamed.len());
    // A target the dog cannot name is a row, not a skip: nothing polls it
    // and nothing restores it, and the operator has to be the one to rename
    // it.
    for dir in found.unnamed {
        // The key is the path's `Debug` form: escaped, so it prints as one
        // line, and exact, so two such directories never share a mute entry
        // (`display` is lossy and folds distinct non-UTF-8 names together).
        // `report` puts the key at the front of the line, so the message
        // does not name the directory again.
        results.push((
            format!("{dir:?}"),
            Err(Error::Config(
                "holds a deploy.toml, and its name cannot be a sheep's (not valid UTF-8, or \
                 carrying a character that would rewrite a log line), so it is neither polled \
                 nor restored. `shep flock` lists every sheep and `shep describe <sheep>` its \
                 cwd: check none runs from inside it, then rename the directory"
                    .to_owned(),
            )),
        ));
    }
    for name in found.named {
        let tree = Tree::for_sheep(shep_home, &name);
        let mut state = match State::read(&tree.state_file()) {
            Ok(state) => state,
            Err(err) => {
                results.push((name, Err(err)));
                continue;
            }
        };
        // Above the `due` check; `smit::publish` says why. A failure does not
        // stop the deploy, and is a row of its own.
        let painted = smit::text(&state);
        let refused = match smit::publish(daemon, &name, &state).await {
            Ok(()) => false,
            Err(err) => {
                results.push((format!("{name}/smit"), Err(err)));
                true
            }
        };
        if !due(&state) {
            continue;
        }
        // Guarded, because this loop's one promise is that a target's
        // failure does not stop the others, and a panic is the one failure
        // `unattended`'s `Result` cannot carry. Nothing in the crate panics
        // on purpose outside tests; this is for the day something does, in
        // a loop meant to run for months.
        let deploy = Box::pin(deploy::unattended(daemon, &tree, &mut state, config));
        let outcome = match CatchUnwind(deploy).await {
            Ok(outcome) => outcome,
            Err(payload) => Err(Error::Panicked {
                sheep: name.clone(),
                what: panic_message(payload.as_ref()),
            }),
        };
        // One `/smit` row per target per tick: a second identical refusal
        // would count twice against the mute.
        if let Err(err) = smit::repaint(daemon, &name, &painted, &state).await
            && !refused
        {
            results.push((format!("{name}/smit"), Err(err)));
        }
        results.push((name, outcome));
    }
    Tick { targets, results }
}

/// What one tick found and did.
struct Tick {
    /// How many targets were on disk, due or not, or `None` when the
    /// directory could not be listed at all.
    ///
    /// Kept apart from the results because a manual target produces no
    /// result and is still a target: a tick with results and no targets is
    /// impossible, and a tick with targets and no results is the ordinary
    /// quiet one. `None` is kept apart from `Some(0)` because "gone" and
    /// "unreadable" want different rows.
    targets: Option<usize>,
    /// One row per target that had something to say, plus a row per smit
    /// that could not be published.
    results: Vec<(String, Result<Outcome, Error>)>,
}

/// Polls forever, deploying what has moved.
///
/// The first tick runs at once and the sleep comes after it, so a restarted
/// dog is doing its job immediately rather than looking broken for an
/// interval first.
///
/// What it says per target is [`report()`]'s, and how often it repeats itself
/// is [`worth_saying`]'s.
///
/// Takes the [`Reader`] rather than a [`DogConfig`], because the section is
/// read again at the top of every tick: an operator who changes `interval`
/// or `retention` is picked up by the next tick, without a restart.
/// A read that fails keeps the section that last parsed and is reported as
/// a row of its own; [`config::Reader::refresh`](Reader::refresh) says why
/// that is not what the startup read does.
///
/// # Errors
/// Never returns `Ok`, and in practice never returns at all: a target's
/// failure is reported and the loop goes on to the next one, because a dog
/// that exited on the first unreachable remote would stop deploying
/// everything else and nothing would restart it with a reason anybody could
/// read. The `Result` is the signature the caller wants, not a promise that
/// something in here can fail outward.
///
/// # Cancellation
/// Stopping mid-deploy is not interrupted at a step of this loop's
/// choosing. A `tokio::select!` around this future cancels it at its next
/// await point, which can be inside a deploy, and that is acceptable: every
/// step is either before the swap, where nothing has been disturbed, or
/// inside the reload, where the shepherd already has its instructions and
/// finishes on its own. What a cancellation can lose is the record write
/// after a verified deploy, which leaves `deploy.toml` naming the previous
/// release and the next tick deploying the same sha again. That costs one
/// rebuild and repairs itself, which is the right shape of failure for a
/// signal that means "stop now".
///
/// It is not deferred by a git call any more. Every git call and the
/// artifact copy run on tokio's blocking pool (`shared::off_thread`), so a
/// stop arriving during a five-minute fetch is answered at once: this
/// future is dropped, `main` gives the pool a second to wind down, and the
/// fetch's own git process finishes or times out on its own after the dog
/// has gone. A fetch against a host that is not answering is still bounded
/// by `DogConfig::git_timeout`, so it fails that one target like any other
/// error rather than holding the loop.
pub async fn run<D: Daemon>(daemon: &D, shep_home: &Path, config: Reader) -> Result<(), Error> {
    run_with(
        daemon,
        shep_home,
        config,
        &mut io::stdout(),
        &mut io::stderr(),
    )
    .await
}

/// [`run`], writing to somewhere a test can read.
///
/// The whole of what this loop does that anybody sees is what it writes, so
/// the two streams are arguments rather than macros: with `println!` baked
/// in, deleting every line of output left the suite green.
///
/// # Errors
/// As [`run`]. A write that fails is NOT one of them: a dog whose log pipe
/// has gone should carry on deploying rather than die of it, so a failed
/// write is passed over, exactly as `println!` would have carried on after
/// a full disk.
async fn run_with<D: Daemon, O: Write, E: Write>(
    daemon: &D,
    shep_home: &Path,
    mut config: Reader,
    out: &mut O,
    err: &mut E,
) -> Result<(), Error> {
    let mut previous: BTreeMap<String, Repeat> = BTreeMap::new();
    let deploy_dir = shep_home.join("deploy").display().to_string();
    let dogs_file = shep_home.join("dogs.toml").display().to_string();
    loop {
        // Ahead of the tick rather than after it, so the deploys this tick
        // makes and the sleep that follows them both run on what the
        // section says now. A change therefore takes effect on the next
        // tick, which is the soonest anything here could act on it. NOT
        // within one interval: this tick can be a whole deploy, and an edit
        // written just after it started waits for it to finish and then for
        // the sleep the old interval asked for.
        let stale = config.refresh(daemon).await;

        let Tick {
            targets,
            mut results,
        } = tick(daemon, shep_home, config.current()).await;

        // A row rather than a bare `eprintln!`, for the reason the smit
        // refusal above is one: a section left broken would otherwise say
        // so every interval forever. Keyed on the file an operator has to
        // edit to fix it, which no sheep name can collide with.
        if let Some(why) = stale {
            results.push((dogs_file.clone(), Err(stale_section(&why))));
        }

        // A deploy directory that is gone answers as an empty list, which is
        // also what a shepherd with no targets yet answers, so the loop went
        // silent the moment a directory was unmounted or removed from under
        // it. Said on every tick with nothing to poll, and left to the mute
        // to say once and then hourly: that covers a dog restarted after the
        // directory went, which a "had targets last tick" flag did not. A
        // listing that failed carries its own row and is not this case.
        if targets == Some(0) {
            results.push((
                deploy_dir.clone(),
                Err(Error::Config(format!(
                    "no deploy targets under {deploy_dir}: nothing to poll until one is set up \
                     with `shep-deploy setup <sheep>`"
                ))),
            ));
        }

        // A target that is gone stops muting anything. Otherwise a target
        // deleted and recreated has its first line swallowed by the entry
        // its predecessor left behind.
        previous.retain(|name, _| results.iter().any(|(seen, _)| seen == name));

        for (sheep, outcome) in results {
            let Some(said) = report(&sheep, &outcome) else {
                // Nothing to say means nothing to repeat: a target that is
                // up to date has put whatever it was complaining about
                // behind it.
                previous.remove(&sheep);
                continue;
            };
            let Some(swallowed) = worth_saying(&mut previous, &sheep, said.text()) else {
                continue;
            };
            let repeats = if swallowed == 0 {
                String::new()
            } else {
                format!(" (repeated {swallowed} times since it was last said)")
            };
            let _ = match &said {
                Said::Note(text) => writeln!(out, "{text}{repeats}"),
                Said::Complaint(text) => writeln!(err, "{text}{repeats}"),
            };
        }

        sleep(config.current().interval).await;
    }
}

/// The complaint a section that could not be re-read is reported as.
///
/// It says what the dog did about it, because the same mistake at startup
/// stops the dog dead: a line naming only the mistake would read as a dog
/// that had stopped, and an operator would go looking for one.
///
/// [`Error::Config`]'s own wording is unwrapped rather than nested,
/// because this is itself a `Config` and two "bad deploy configuration"
/// prefixes on one line read as a fault in the dog rather than one in the
/// file.
fn stale_section(why: &Error) -> Error {
    let what = match why {
        Error::Config(what) => what.clone(),
        other => other.to_string(),
    };
    Error::Config(format!(
        "{what}. The dog is still polling on the section it last read, so it is not yet \
         running what this file now says"
    ))
}

#[cfg(test)]
mod tests;
