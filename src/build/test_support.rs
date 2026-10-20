//! Helpers shared by the tests of the `build` modules.

use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::fixtures;

use super::{BuildSpec, run};

/// A build budget the test tier can never legitimately hit.
///
/// Every build here is `true` or a one-line shell command, so anything
/// slower than a minute is a hang worth failing on rather than waiting
/// out. Distinct from the production default, which is an hour because a
/// cold build of a real workspace legitimately runs tens of minutes.
pub(super) const TEST_BUILD_BUDGET: Duration = Duration::from_secs(60);

/// Runs a build with every knob at its default: sheep `"web"`, no
/// `as_user`, no passthrough, a fresh cache tempdir, and
/// `TEST_BUILD_BUDGET`.
///
/// Exists because this module's own `run` calls had started to drift the
/// way `fixtures.rs`'s module doc describes for `run_git` and
/// `head_of`: one nine-line block, copied a couple dozen times, one
/// budget or one cache dir away from its neighbor by accident rather
/// than intent. A test that needs a specific cache, user, passthrough or
/// budget still calls `run` directly instead of going through this.
pub(super) async fn build(rel: &Path, spec: &BuildSpec) -> Result<(), Error> {
    run(
        "web",
        rel,
        spec,
        None,
        &[],
        fixtures::tempdir().path(),
        TEST_BUILD_BUDGET,
    )
    .await
}
