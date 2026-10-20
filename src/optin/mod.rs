//! Opt-in: taking a git checkout shep already runs and turning it into a
//! deploy target, then cutting the sheep over to it.
//!
//! Two halves, and the line between them is the only thing in this crate
//! with no rollback. [`prepare_with()`] builds the tree and the first release, and
//! [`cut_over`] registers the sheep against `current` and removes the
//! instances it replaced.
//!
//! **Nothing [`prepare_with()`] does touches the flock.** A failure anywhere in it
//! leaves the sheep running exactly as it was, from the operator's own
//! checkout, with nothing about the flock changed. What it leaves behind on
//! a failure is a directory, and the operator's fix is to run the command
//! again once they have fixed whatever it named.
//!
//! **Everything [`cut_over`] does is past that line.** The moment the
//! shepherd accepts its `Start` a process has been spawned and a config has
//! been recorded against the sheep's name, and those are two separate things
//! to put back: see [`undo::undo_start`].
//!
//! # Why this ordering is load-bearing, not tidy
//!
//! `origin_cwd`, `origin_script` and `previous_config` are all read from the
//! shepherd's muster roll, and the roll is poisoned the instant a `Start` is
//! accepted: `FlockRegistry` is name-keyed, so from that moment the roll's
//! entry for this sheep is the DOG's own config, not the operator's.
//! [`prepare_with()`] captures all three before it ever sends one, and the refusal
//! at its own top is what stops a re-run after an abandoned cutover from
//! capturing the deploy tree as the thing to restore to.
//!
//! # Why `git init --bare` and a fetch, rather than a clone
//!
//! [`crate::git::fetch`] is anonymous by URL with a mirror refspec and
//! `--prune`, and needs no configured remote - which is why `git_dir` never
//! gets a `git remote add`. An empty bare repository plus that same fetch
//! reaches exactly the state a clone would, through the one code path the
//! poll loop already uses every 30 seconds, rather than through a second
//! one that would only ever run once. The cost is real and worth naming:
//! opt-in downloads the repository from the remote rather than hardlinking
//! from the operator's checkout next door. Cloning locally would be
//! faster, and it was rejected because it entangles the dog's object store
//! with a checkout the design says the dog only ever reads, and because a
//! first-run-only code path is the one nothing exercises.

mod attempt;
mod cutover;
mod prepare;
#[cfg(test)]
mod test_support;
mod undo;

pub use cutover::cut_over;
pub use prepare::prepare_with;
