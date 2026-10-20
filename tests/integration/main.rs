//! The tier that drives a REAL shepherd.
//!
//! Every module under `src/` is unit-tested against a fake [`Daemon`] or a
//! throwaway git fixture. None of that proves the binary can actually talk
//! to a shepherd - reload a real sheep, read a real `describe`, or leave a
//! real release serving when a build fails. That is what this tier is for.
//!
//! Gated behind the `integration` feature, and needing `$SHEP_BIN` pointed at
//! a built `shep`:
//!
//! ```text
//! SHEP_BIN="$(command -v shep)" cargo test -p shep-deploy --features integration
//! ```
//!
//! `shep-deploy` is a binary crate with no library, so this tier cannot call
//! `crate::deploy::deploy` the way the unit tests in `src/deploy/` do -
//! everything here goes through the two real binaries, `shep` and
//! `shep-deploy` itself, exactly as an operator would run them.
//!
//! # $SHEP_HOME is a temporary directory in every test here
//!
//! Not because a developer's real `~/.shep` is production - it is not, on
//! this machine - but for the ordinary reason any test suite isolates its
//! fixtures: tests run in parallel, and two tests sharing one `$SHEP_HOME`
//! would collide on the same socket, the same `deploy/web` tree, and the
//! same flock entry. A test that depends on another test's leftover state is
//! not a test. Every [`Shepherd`] below owns its own `tempfile::tempdir` and
//! kills the daemon it booted when it drops.

mod deploy;
mod optin;
mod poll;
mod reload;
mod support;
