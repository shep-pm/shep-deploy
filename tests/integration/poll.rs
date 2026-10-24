//! The supervised dog deploying on its own tick.

use std::fs;
use std::time::Duration;

use crate::support::fixtures::*;
use crate::support::shepherd::*;

/// How long the poll test gives the supervised dog to notice a branch that
/// moved between two of its own ticks.
///
/// This number is an assertion rather than a safety margin, and it is
/// bracketed on both sides. Below it: the test configures
/// `interval = "1s"`, and the deploy it triggers is a local fetch, a
/// worktree, a swap and a reload, which the rest of this tier does in about
/// two seconds. Above it: `crate::config`'s `DEFAULT_INTERVAL` is thirty
/// seconds, so a dog that never found its own `[deploy]` section cannot
/// get inside this however healthy it is. That gap is the only thing making
/// the section's interval observable at all - see
/// [`the_supervised_dog_deploys_a_moved_branch_without_being_asked`].
const POLL_WINDOW: Duration = Duration::from_secs(20);

/// The default `crate::config` falls back to when a dog cannot name itself,
/// restated because a test binary cannot see a binary crate's internals.
///
/// The assertion below is the whole point. `POLL_WINDOW` sits between a
/// measured two seconds and this thirty, and that bracket is what makes the
/// configured interval observable. Lower this toward twenty and the test stops
/// distinguishing a dog that read its section from one that did not, then
/// starts flaking, in that order. Neither failure names its cause, so the
/// relationship is asserted at compile time rather than described.
const DEFAULT_INTERVAL_SECS: u64 = 30;

const _: () = assert!(
    POLL_WINDOW.as_secs() + 5 <= DEFAULT_INTERVAL_SECS,
    "POLL_WINDOW must stay under config::DEFAULT_INTERVAL with room to spare, \
     or a defaulted dog's next tick can land inside the window and the test \
     stops telling the two apart"
);

/// fails if the supervised mode does not deploy on its own, or does it on a
/// configuration it never read.
///
/// `watch = "auto"` means nothing until the binary, given NO ARGV AT ALL as
/// the dog contract requires, connects, works out its own adopted name from
/// its own pid in a real flock listing, reads the `[dog.<name>]` section
/// that name selects, finds its targets on real disk and deploys one. Every
/// one of those is stubbed in the unit tests, and the pid lookup in
/// particular can only be wrong against a real daemon.
///
/// # The order of the last three steps is the whole test
///
/// An earlier version of this adopted the dog AFTER moving the branch, and
/// it covered none of the self-identification it claimed to. `adopted_name`
/// returning `None` unconditionally passed it in 1.25 seconds, because
/// `crate::config` answers a nameless dog with the documented defaults, the
/// loop ticks once before it ever sleeps, and by then the branch had already
/// moved. Every observable was identical whether the section was read,
/// misread, or never found - and the same blind spot swallowed
/// `Request::DogConfig` and `DogConfig::parse` along with it.
///
/// So the branch moves LAST, strictly between two ticks, and the deploy is
/// required inside [`POLL_WINDOW`]. That makes the interval the thing being
/// measured: at the configured second the dog gets there with time to spare,
/// and at the thirty-second default it cannot get there at all. A dog that
/// never found its section now fails this rather than passing it.
///
/// Waiting for the tick before the branch moves is what makes that
/// bracketing sound, and an up-to-date tick says nothing an operator or a
/// test could read - `report` is silent on `UpToDate` by design. What it
/// does leave is `FETCH_HEAD`: `deploy::go` fetches before it compares
/// anything, so the file reappearing is a tick that ran to completion and
/// found nothing to do.
///
/// `dogs.toml` is written before the adopt, not after, because the dog reads
/// its section once at startup rather than per tick. `dogs.toml`, not
/// `shep.toml`: shep 0.1.32 moved every dog's section there, under its bare
/// name, and migrates a `[dog.<name>]` still in `shep.toml` only at boot.
/// This test writes after the shepherd is up, so a section written to the
/// old file would never be read, and the dog would run at its default
/// interval and fail the window below. That is what happened the day shep
/// 0.2.0 shipped.
///
/// # Why the dog's own stdout is asserted on as well
///
/// Not belt and braces. `shep adopt` vets a candidate by spawning it with no
/// argv and the real `$SHEP_HOME` before it records anything, so for about
/// fifty milliseconds a second copy of this binary is running the same poll
/// loop against the same targets. That copy is killed with its stdio on
/// `/dev/null`, so a line in the dog's log can only have come from the
/// instance the shepherd supervises. The running release alone would not
/// tell the two apart, and neither would a mutation of the loop, which the
/// vetting spawn runs too.
#[test]
fn the_supervised_dog_deploys_a_moved_branch_without_being_asked() {
    let shepherd = Shepherd::new();
    let origin = origin_with_app("v1", Readiness::Probe, "");
    let sha = build_tree(shepherd.home(), "web", origin.path());
    write_state(shepherd.home(), "web", origin.path(), &sha);
    register_web(&shepherd, Readiness::Probe, "probed", "");
    wait_until("web to come up", || described_online(&shepherd, "web"));
    let out_log = out_file(&shepherd, "web");

    // A second rather than the default half minute, and the whole test is
    // built to notice the difference.
    fs::write(
        shepherd.home().join("dogs.toml"),
        "[deploy]\ninterval = \"1s\"\n",
    )
    .expect("write dogs.toml");

    // Adopted and supervised, with no argv, exactly as the contract says.
    // The name is the binary's own stem with `shep-` stripped, which is what
    // makes the section above this dog's.
    shepherd.ok(&["adopt", DEPLOY_BIN, "--style", "bare"]);
    wait_until("the dog to be supervised", || {
        described_instances(&shepherd, "deploy") == 1
    });
    let dog_log = out_file(&shepherd, "deploy");

    // Removed after the dog is up, so the vetting spawn is long dead and
    // cannot be what puts it back.
    let fetch_head = shepherd.home().join("deploy/web/git/FETCH_HEAD");
    fs::remove_file(&fetch_head).expect("remove FETCH_HEAD");
    wait_until("a tick that looked at the remote and found nothing", || {
        fetch_head.is_file()
    });

    // Only now, so the next tick is one configured interval away.
    write_run_script(origin.path(), "v2");
    git(origin.path(), &["commit", "-qam", "v2"]);
    let second = head_of(origin.path());

    wait_within(
        "the poll loop to deploy v2 on its own, within one configured interval",
        POLL_WINDOW,
        || last_line(&out_log).as_deref() == Some("v2"),
    );
    wait_within(
        "the supervised dog to report the deploy as its own",
        POLL_WINDOW,
        || {
            fs::read_to_string(&dog_log)
                .unwrap_or_default()
                .contains(&format!("web deployed {second}"))
        },
    );
}
