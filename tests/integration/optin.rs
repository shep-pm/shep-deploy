//! Taking over a sheep an operator already runs, through `setup`.

use std::fs;
use std::path::Path;

use crate::support::fixtures::*;
use crate::support::shepherd::*;

/// The `listen_timeout` the pre-adoption checkout registration declares, in
/// seconds. Short: nothing in that phase is being measured, and the test
/// spends the wait before it can do anything else.
const CHECKOUT_LISTEN: u64 = 1;

/// Registers `web` the way an operator already runs it, before this dog has
/// touched anything: from their OWN clone of `origin`, out of a Flockfile
/// with **no `cwd` key at all**.
///
/// The missing `cwd` is the point rather than a shortcut. It is the
/// ordinary way an app is registered, and it is precisely the case the
/// cutover's explicit `cwd` exists for: shep resolves a defaulted `cwd` to
/// the Flockfile's own directory at REGISTRATION, so the sheep this starts
/// is pinned to this checkout and [`crate::optin::cut_over`] has to re-point
/// it at the `current` symlink itself.
///
/// The clone is separate from `origin` on purpose. This Flockfile is
/// probeless, because the probe the committed one carries looks through a
/// `current` symlink that does not exist yet and the sheep could never come
/// up; leaving that edit in `origin`'s working tree would then get committed
/// by the next `git commit -a` and ship a release the deploy path refuses
/// for having no probe.
///
/// Returns the clone, which the caller has to keep alive: it is the
/// registered sheep's working directory until the cutover moves it.
fn register_from_checkout(shepherd: &Shepherd, origin: &Path) -> tempfile::TempDir {
    let checkout = tempfile::tempdir().expect("tempdir");
    git(
        checkout.path(),
        &[
            "clone",
            "-q",
            origin.to_str().expect("utf-8 origin path"),
            ".",
        ],
    );
    // The operator's own override, with the probe, sits beside their
    // checkout's Flockfile: the deploy links it in from here, since this
    // clone is the checkout `setup` records. The committed `.gitignore`
    // keeps it out of every commit, which is what makes it theirs.
    fs::write(
        checkout.path().join("Flockfile.override.toml"),
        format!("[[app]]\nname = \"web\"\n{}", probe_toml(shepherd.home())),
    )
    .expect("write the operator's override");
    let path = checkout.path().join("Flockfile.toml");
    fs::write(
        &path,
        app_toml(
            shepherd.home(),
            false,
            Readiness::Heuristic(CHECKOUT_LISTEN),
            "",
        ),
    )
    .expect("write the checkout Flockfile");
    shepherd.ok(&[
        "start",
        path.to_str().expect("utf-8 path"),
        "--style",
        "bare",
    ]);
    checkout
}

/// fails if a sheep taken over by `setup` does not follow later swaps.
///
/// This is the fact no fake can check and the one that fails silently
/// exactly one release after the mistake: a Flockfile app's DEFAULTED cwd is
/// resolved at REGISTRATION, so a sheep registered from inside a release is
/// pinned to that release forever and every later swap moves a symlink it no
/// longer reaches. Measured on a real shepherd before this crate existed:
/// the reload after a swap re-ran the OLD release's script.
///
/// It deploys TWICE for that reason. One deploy proves nothing here, because
/// a sheep pinned to release one and a sheep following `current` are
/// indistinguishable while release one IS current.
///
/// It also pins two things no unit test can reach: that `Start` on a
/// registered name adds an instance beside the old one rather than replacing
/// it, and that the cutover deleted the ORIGINAL instance and not the
/// newcomer. Both are real rows under one name on a real shepherd, and an id
/// read from the wrong side of the `Start` deletes the release that was just
/// deployed while every count assertion still passes. The surviving pid is
/// what tells them apart.
#[test]
fn a_sheep_taken_over_by_setup_follows_a_later_swap() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app(shepherd.home(), "v1", Readiness::Probe, "");

    // The sheep as the operator already runs it: registered from their own
    // checkout, with no deploy tree anywhere. Held, because it is that
    // sheep's working directory until the cutover moves it.
    let _checkout = register_from_checkout(&shepherd, origin.path());
    wait_until("web to come up from the checkout", || {
        described_online(&shepherd, "web")
    });
    let before = described_pid(&shepherd, "web").expect("a pid");

    let setup = shepherd.deploy_args(&["setup", "web"]);
    assert!(
        setup.status.success(),
        "setup failed: {}{}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );

    // One instance again, and it must be the NEWCOMER. Two rows shared this
    // name for the length of the cutover, so this is the assertion that
    // catches an id taken from the wrong side of the `Start`.
    wait_until("the cutover to settle", || {
        described_instances(&shepherd, "web") == 1
    });
    assert_ne!(
        described_pid(&shepherd, "web"),
        Some(before),
        "the surviving instance must be the newcomer, not the one it replaced"
    );

    // Read after the cutover settled, so it names the newcomer's log rather
    // than the deleted original's.
    let out_log = out_file(&shepherd, "web");

    // Now the half a single deploy cannot prove.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);
    let deployed = shepherd.deploy("web");
    assert!(
        deployed.status.success(),
        "the second deploy failed: {}{}",
        String::from_utf8_lossy(&deployed.stdout),
        String::from_utf8_lossy(&deployed.stderr)
    );

    wait_until("the second release to be serving", || {
        last_line(&out_log).as_deref() == Some("v2")
    });
}
