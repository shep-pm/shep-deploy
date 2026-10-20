//! Helpers shared by every module of the integration tier.
//!
//! A copy kept apart from `src/fixtures.rs` on purpose: this target cannot see
//! a binary crate's internals.

pub mod fixtures;
pub mod shepherd;
