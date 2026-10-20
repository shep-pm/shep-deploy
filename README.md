# shep-deploy

[![Crates.io Version](https://img.shields.io/crates/v/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![License](https://img.shields.io/crates/l/shep-deploy.svg)](https://github.com/shep-pm/shep-deploy#license)
[![MSRV](https://img.shields.io/crates/msrv/shep-deploy.svg)](https://crates.io/crates/shep-deploy)
[![CI](https://github.com/shep-pm/shep-deploy/actions/workflows/test.yml/badge.svg)](https://github.com/shep-pm/shep-deploy/actions/workflows/test.yml)

A deploy dog for [shep](https://github.com/shep-pm/shep).

Watches a git branch, builds a release in an isolated directory, swaps to it,
reloads the sheep, and rolls back on its own if the new release does not come
up.

It is an external dog, the same shape as
[shep-log-rotate](https://github.com/shep-pm/shep-log-rotate): an ordinary
binary you adopt, talking to the daemon over the socket the CLI already uses.

## Install

```bash
cargo install shep-deploy
shep adopt shep-deploy
```

Upgrading later wants `--force`:

```bash
cargo install shep-deploy --force
shep restart deploy
```

That flag is not politeness. This dog links `shep-client`, and its own version
does not change when that does, so after you upgrade shep the rebuild you need
is exactly the one cargo declines to do: it prints `already installed, use
--force to override`, builds nothing, and exits 0. It looks like it worked.

`shep adopt` registers it with the shepherd, which supervises it from then on.
`shep dogs` lists what you have adopted.

Adopting runs the binary twice before anything is recorded. `shep-deploy
--version` answers with the crate version and the shep protocol number this
build speaks, which is what shep checks against its own floor: a dog built
below it is refused there and then, rather than at its first handshake.
`shep-deploy --schema` answers with a JSON Schema for the `[deploy]` section
below. `shep restart deploy` asks the first question again, to see whether the
binary on disk has moved ahead of the process still running.

## Telling it how to build

The build command lives in the deployed repository's own Flockfile, under the
table shep keeps for a dog's config:

```toml
[[app]]
name = "web"
script = "server.js"

[dog.deploy.build]
command = "npm ci && npm run build"
artifacts = ["dist/bundle.js"]
```

shep reads nothing under `dog` and validates none of it. It only had to stop
refusing the document for carrying it, which it does as of 0.1.10, so the same
Flockfile now registers with `shep start` and tells this dog how to build.

That is a change from earlier versions, which put the block at the top level as
`[build]`. shep refused a Flockfile with an unknown top-level key, so an
operator following these instructions could not register their app at all. A
top-level `[build]` is now refused here by name, pointing at the new spelling,
rather than ignored and silently building nothing.

## Running a migration before the swap

A release hook is a command that runs once per deploy, after the build and its
artifacts and before `current` moves. It exists for a database migration, which
otherwise has to end the build command: its password then reaches every script
the build runs, and its output sits unlabelled in the build log.

```toml
[dog.deploy.release]
command = "supabase db push"
env = { SUPABASE_DB_PASSWORD = "..." }
```

- It belongs in `Flockfile.override.toml`, which is yours. A committed
  `Flockfile.toml`, or an override the dog did not link in from your checkout,
  that carries `[dog.deploy.release]` is refused by file and table: the hook's
  `env` holds your secrets, and a commit must not choose the command that
  receives them. `[dog.deploy.after]` is refused the same way.
- It runs in the release, as the app's `user`, through `sh -c`, with the build's
  cleared environment: the base set, the dog's `passthrough`, then the hook's
  own `env`. The dog sets `SHEP_DEPLOY_SHEEP`, `SHEP_DEPLOY_SHA` and
  `SHEP_DEPLOY_RELEASE` (an absolute path) last, so the hook's `env` cannot
  override them.
- It is bounded by `build_timeout` and killed as a process group, like the
  build. The log gets one line, `shep-deploy: <sheep>: running the release
  hook`, before it starts.
- A non-zero exit, a signal or a timeout stops the deploy. `current` does not
  move, the old release keeps serving, and the sha is held as for a failed
  build.
- `setup` runs no hook. A cutover does not touch the app's database.

A rollback does not undo a migration. If the new release fails verification
after the hook ran, the old release comes back against the migrated database.
A held sha that you retry with `shep deploy <sheep>` runs the hook again, so
write the command to be idempotent.

## Deploying only what a push touches

Several services can share one repository, each its own target. Every push to
the branch then reloads all of them, so a change to one restarts the rest. A
target that lists the paths it runs from is left alone by a push that changes
none of them:

```toml
[dog.deploy.reload]
paths = ["cmd/voting", "internal/voting", "pkg", "go.mod"]
```

- Entries are git pathspecs. A directory name matches everything under it, and
  a glob such as `assets/*.png` works too.
- `Flockfile.toml` always counts, so a push that edits the table deploys.
- The paths come from the release that is live now, not from the new commit.
  A push that changes the table is still judged by the old one, and it deploys
  because `Flockfile.toml` changed.
- No table, or `paths = []`, deploys every new sha, as before. An unknown key is
  refused by name.
- A skipped push costs a fetch and a diff: no checkout, build or reload, and
  `deploy.toml` is not written. The log says `<sheep> skipped <sha>, nothing
  under its reload paths changed`. It is said once, then again about once an
  hour while the same push stays skipped, and at once when the branch moves.
- The next push is compared with the last sha that was deployed, not with the
  one skipped, so a skipped push's changes still count toward it.
- If the dog cannot tell (the deployed sha is gone from its clone, the live
  Flockfile is unreadable, git fails) it deploys and says why.
- `shep-deploy deploy <sheep>` by hand never skips. It is you asking for a
  deploy now.

## Purging a cache after the swap

An after hook is a command that runs once per deploy, after the new release is
swapped in, verified and recorded. A build cannot do this work: it runs before
the swap, so a purge from the build refills from the old release.

```toml
[dog.deploy.after]
command = "nginx -s reload && ./purge-cloudflare.sh"
env = { CLOUDFLARE_API_TOKEN = "...", CLOUDFLARE_ZONE_ID = "..." }
```

- It is set in `Flockfile.override.toml` and refused anywhere else, as the
  release hook is.
- It runs in the new release, as the app's `user`, through `sh -c`, with the
  release hook's environment. `SHEP_DEPLOY_SHEEP`, `SHEP_DEPLOY_SHA` and
  `SHEP_DEPLOY_RELEASE` name the sheep, the new sha and the release's absolute
  path. `current` already points at that release and `deploy.toml` already
  names the sha.
- It is bounded by `build_timeout` and killed as a process group. The log gets
  one line, `shep-deploy: <sheep>: running the after hook`, before it starts.
- It runs inside the deploy, so no other deploy of that target starts until it
  ends.
- It runs only if the record write succeeded. If `deploy.toml` cannot be
  written, the log says the hook was skipped.
- It never runs for a deploy that is rolled back, that fails before the swap,
  or that finds nothing new, and `setup` runs none.
- A failure changes nothing. A non-zero exit, a signal or a timeout is logged
  as `shep-deploy: <sheep>: deployed <sha>, but the after hook failed: ...`.
  The release stays live, the outcome is still deployed and the sha is not
  held. The hook is not retried; run the purge by hand if it matters.

## What one deploy does

1. Fetch into a bare clone, and compare the branch head to the last deployed
   sha.
2. `git worktree add` the new sha, sharing the object store.
3. Symlink the shared files in: whatever git ignores and `.shepignore` does
   not. A `.shepignore` line is a bare name, matched at any depth, or a path
   with a `/` in it, anchored to the checkout. A leading `/` anchors too, a
   leading `./` and a trailing `/` are dropped, and `\!name` names a file
   that begins with `!`, as in `.gitignore`. A glob, a bare `!` negation, any
   other backslash escape or a `..` is refused by name rather than matched
   against nothing. A linked path that has a `node_modules`, `.output`, `dist`
   or `.next` component, or that overlaps a `build.artifacts` entry, is named
   in a warning on stderr each deploy; add it to `.shepignore` to stop
   linking it. A linked file whose length or modified time, or a linked
   directory whose own modified time, changed during the build is named too,
   because the build may have written through the link into your checkout. A
   file the running app writes, a database or a log, is named as well; leave
   those linked.
4. Run the build, as the app's `user` if it sets one.
5. Run the release hook, if your override sets one.
6. `rename(2)` `current` onto the new release.
7. `Reload` the sheep.
8. Verify. On failure, put `current` back, reload again, and verify that the
   old release came up. A rollback that does not come up is an error, not a
   rollback.
9. Record the sha in `deploy.toml`, then run the after hook, if your override
   sets one. Its failure is logged and does not undo the deploy.

Steps 1 to 5 never touch the running app. A build that fails costs a
directory, not an outage. A hook acts on whatever it was written to act on,
which is why a rollback cannot undo it.

Verification waits for a *new* process to reach `Online`, not for any process
to be `Online`. shep answers a reload before it has finished one, and keeps
the old instance when the replacement never becomes ready, so "something is
online" is true throughout a deploy that failed.

## Layout

Everything lives under `$SHEP_HOME/deploy/<sheep>/`:

```text
git/                 one bare clone, shared by every release
releases/<sha>/      a worktree per release
current -> releases/<sha>
deploy.toml          remote, branch, deployed sha, held sha, staged sha, verify mode,
                     watch mode, and the app as it was before adoption (owner-only: it holds env)
```

The sheep's `cwd` is `current`, permanently. Set it explicitly when you
register the app: a Flockfile `cwd` left to default is resolved at
registration, which pins the sheep to one release and makes every later swap
invisible to it.

## Commands

```sh
shep-deploy deploy <sheep>
shep-deploy deploy <sheep> --watch auto|manual
shep-deploy deploy <sheep> --branch <name>
shep-deploy deploy <sheep> --branch refs/pull/1/head
shep-deploy deploy <sheep> --stage
shep-deploy deploy <sheep> --promote
shep-deploy setup <sheep>
shep-deploy setup <sheep> --verify probed|alive
shep-deploy survey
shep-deploy on-remove
shep-deploy help
```

`--watch` changes the setting and returns without deploying. `--branch` does
the same for the branch a target tracks, which `setup` read from the checkout
once; it clears a held sha and refuses a target `setup` never finished. A name
the remote lacks is reported by the next tick. `--branch` also takes a full ref
beginning `refs/`, such as `refs/pull/1/head`, which the target then follows on
every push like a branch. Only that ref is fetched, not every pull ref the
remote has. A closed pull request's ref stays on GitHub, so the target keeps
serving its last head. `--stage` fetches, builds the branch head and records it
in `deploy.toml` as `staged`, then returns: it never swaps `current`, reloads
the sheep, or changes `deployed` or `failed`. It is refused on a `watch = auto`
target, because the next tick would deploy that head anyway; set `--watch
manual` first. Staging the head that is already deployed, or already staged with
its release still built, does nothing; a staged release that is gone is built
again. Retention spares the staged release, and a deploy that lands clears
`staged`. While a sha is staged, a shep-deploy from before the field refuses to
read the record. `--promote` lands what was
staged: it runs the release hook, swaps `current`, reloads the sheep, verifies
it and runs the after hook, with the rollback a deploy has, and builds and
fetches nothing. It lands the staged sha even if the branch has moved since,
and it is refused when nothing is staged or the staged release is gone, with
`--stage` as the way out. A promote that does not come up is rolled back and
leaves `staged` set, so it can be run again. It ignores a held sha, as a
deploy asked for by name does. `--verify` on
`setup` records how the sheep's deploys are verified, `probed` when it is
left out. `survey` reports
where every registered sheep stands and starts, registers and writes nothing.
`help`, `-h` and `--help` print usage on stdout. A sheep named `help` is reached
with `shep deploy deploy help`.

## Running as a dog

Adopted as a dog, `shep-deploy` takes no arguments and polls instead, but only
when `SHEP_DOG_NAME` is set, as shep does when it starts the dog. Every 30
seconds by default, it deploys any `watch = "auto"` target whose branch has
moved. Configure it in `$SHEP_HOME/dogs.toml`:

```toml
[deploy]
interval = "30s"
retention = 5
git_timeout = "5m"
build_timeout = "1h"
passthrough = ["CARGO_HOME"]
```

All five are read again at the top of every tick, so changing any takes
effect on the next tick, with no restart. Not within one interval: a tick
that is running a deploy finishes it first, and only then sleeps the
interval it read before the edit. A section that stops parsing
while the dog runs leaves it polling on the last one that did, and it says
so; the same mistake at startup stops the dog instead, because there is
nothing else for it to run on then. A shep older than 0.1.32 read the same
keys from `[dog.deploy]` in `shep.toml`; a newer one moves that section into
`dogs.toml` the next time the shepherd boots.

`shep lookout` renders a form over this section: press `s`, then `e` on the
dog's row. It has one because of the `--schema` answer above. None of the five
keys is a credential, so the pane redacts nothing. `passthrough` names
variables and never carries their values.

`retention` is how many of the newest releases each target keeps. Two are
spared whatever their age: the one `current` points at, and the one
`deploy.toml` names. They are the same release except after a deploy died
between its swap and its record write, so a target holds `retention`
directories in the ordinary case and up to two more after that. It cannot be
below 2: the release a failed deploy rolls back to is the second newest, so
anything lower would silently disable rollback, and it is refused rather than
clamped.

`interval`, `git_timeout` and `build_timeout` are refused under a second. A
bare number is milliseconds, so `interval = "30"` is thirty milliseconds and a
dog fetching thirty times a second; write `"30s"`.

`git_timeout` bounds the fetch, which is the only git call that talks to a network. The rest operate on local directories and cannot hang on a remote. Targets are deployed one at a
time, so without it a remote that drops packets rather than refusing them stops
every target and every smit refresh, with no error and no log line. Five
minutes by default, which is generous enough for a cold clone of a large
repository.

`build_timeout` bounds the build command and both hooks, against the same failure and for the
same reason. A build that never finishes holds that same one-at-a-time loop, so
no other target deploys and nothing is logged, because from the loop's point of
view nothing has gone wrong. An hour by default, which no honest build should
reach: it exists to turn a build that will never finish into an ordinary
per-target failure, not to put a schedule on slow work. A build past it is
killed as a process group, so whatever the build command started goes with it.

`passthrough` names environment variables a build may keep from the dog's own
environment. See Security below for why that list starts empty. An entry that
is not a variable name, or is listed twice, is refused; one that names a
variable the dog was not started with is said at build time, once per build.

One target's failure never stops the others, and never stops the dog. Each
target's outcome is reported on its own and the loop carries on. `up to date`
prints nothing at all, which is the answer to almost every tick of almost
every target, and a line that repeats is said once rather than every interval.

A tick never begins while the previous one is still running, so a push landing
during a build is deployed on the next tick rather than aborting the build in
flight.

Nor does a second process. Each deploy takes an exclusive `flock` on its own
tree for as long as it runs, so `shep-deploy deploy web` typed while the dog is
mid-tick on `web` is refused in one sentence rather than colliding somewhere
inside git. The lock is per sheep, so other targets are unaffected, and the
kernel releases it if the holder dies, so a killed dog leaves nothing behind
holding a sheep hostage.

Every tick also paints each target's smit, so `shep flock` shows which branch
and sha it is on without a second command:

```text
▲ main@a1b2c3      watched
⏸ main@f6e5d4      manual
```

Republished every tick rather than on change: shep holds a smit in memory only
for as long as the connection that painted it stays open, so a dog that only
published on change would show nothing at all after a daemon restart until its
next deploy. A refused smit is logged and otherwise ignored, since it is
cosmetic and never worth failing a deploy over.

A deploy in the dog's own tick repaints the smit as soon as it lands. A smit
belongs to the connection that painted it, so a `deploy`, `setup` or `--watch`
run by hand cannot leave one: the dog paints the new state on its next tick,
which starts `interval` after the current one finishes.

## How it holds its connection

Adopted, the dog announces itself by the name shep gave it, taken from
`SHEP_DOG_NAME`. That is what shep records as a handshake. A dog that does not
send one connects, serves every request correctly, and is still shown as
`silent`, restarted once, and then written off as stale.

The connection survives a handover as well, so `shep upgrade` does not leave
the dog running against a socket nobody is listening on.

A refusal it does not survive. When a newer shepherd rejects the handshake on
protocol skew, no amount of reconnecting fixes it, so the dog reports the
shepherd's version and its refusal message, then exits `5` rather than ticking
on against a connection that will never answer. It cannot name its own protocol
number, because `LinkState::Refused` does not carry one. Upgrade whichever side
is behind and shep starts it again.

Run by hand, `shep-deploy deploy web` announces no name at all. It is not a
dog, and a command claiming to be one would have shep restart the real dog when
the command's own handshake was refused.

A bare `shep-deploy` run by hand, with `SHEP_DOG_NAME` unset, prints usage and
exits `2` rather than polling, because a second loop beside the adopted dog
would deploy the same targets twice. Setting `SHEP_DOG_NAME` yourself starts
the loop and names its handshake. shep sets it for a bare `shep deploy` typed at
a terminal too, so that command still starts a second loop.

## A commit that fails is held

A commit that does not land is left alone until the branch moves. It is
written into `deploy.toml` as `failed`, because otherwise the branch head and
the deployed sha stay different and every tick runs the whole sequence again:
fetch, full rebuild, swap, reload, wait out the verification budget, roll back,
reload again. Two reloads of a live app and a build every thirty seconds, from
one bad commit, until somebody notices.

Pushing a fix clears it, which is what CI does with a red commit; so does
`shep deploy <sheep>`, which retries the same commit deliberately. The tradeoff
is deliberate too: a deploy that failed on a network blip rather than on the
commit waits for one of those two, rather than being retried on the next tick.

`shep-deploy survey` shows such a target as `held`, naming the commit it is
holding, so a target stuck since yesterday does not read like one with nothing
to do. Survey reads the record, never the remote, so a hold that a push already
cleared still shows until the next tick.

## Taking a sheep over

`setup` takes a sheep over: it builds the tree, fetches the repository, links
the shared files in, builds the first release, and re-registers the sheep with
its `cwd` set to `current`. A Flockfile that names no `interpreter` keeps the
one the sheep already ran under: `shep start` fills that in from `shep.toml`'s
`[interpreters]`, and this dog cannot read that map.

An app with neither a `readiness_probe` nor `wait_ready`, a bot with no port
say, is refused before anything is built, because its first deploy would be
refused for the same reason. Take it over with `shep-deploy setup <sheep>
--verify alive`, which records `verify = "alive"` in its `deploy.toml`. A
refused `setup` leaves a directory with the fetched repository and the staged
release but no `deploy.toml` and no `current`, which is not a deploy target, so
running it again is a retry.

The first cutover is the one deploy that may have downtime. It runs two
instances at once, so an app that does not bind with `SO_REUSEPORT` cannot take
its own port while the original still holds it, and the new instance is then
removed and the original left serving. Every deploy after the first replaces
the instance rather than joining it, and does not meet this.

Most apps cannot set `SO_REUSEPORT`, and some cannot even be made to: Node
refuses `reusePort` on macOS outright. For those, stop the sheep before the
cutover and start it afterwards:

```bash
shep stop web
```
```bash
shep-deploy setup web
```

With the port free the newcomer binds and the cutover lands. It costs the
downtime this section already warns about, and it is the difference between a
setup that works and one that cannot. Measured 2026-08-28: the same app that
failed the cutover while running completed it once stopped.

A cutover that was abandoned leaves the tree behind, and a sheep is not a
deploy target until a cutover lands. `shep-deploy deploy` refuses such a target
before anything at all happens, because its record names no deployed release.

That refusal is load-bearing rather than tidy. Without it the deploy would not
stop: it would build, swap, reload the sheep at its own checkout, see a real
turnover, and report success for a release nothing served.

Remove its tree and run `setup` again once the cause is fixed. Both `setup` and
the failure message say this, and both print the resolved path to remove rather
than a `$SHEP_HOME` you would have to expand yourself. `setup` leaves it
`watch = manual` until the cutover lands, and `--watch auto` on one is refused
for the same reason: it would ask for that deploy once every interval,
unattended.

## Verification

`verify = "probed"` (the default) needs the app to have a `readiness_probe` or
`wait_ready`; without one, shep reports a process `Online` for not having died
yet, so there is nothing to verify against and the deploy is refused.
`verify = "alive"` is the deliberate downgrade: a new process, still running
ten seconds later. An app that cannot have a gate, one with no port, is taken
over with `shep-deploy setup <sheep> --verify alive`; `setup` refuses it under
`probed` before it builds anything.

Both modes watch the flock after the turnover. `probed` requires the same
processes, all `Online`, at every look for ten seconds; `alive` sleeps ten
seconds and then polls for `Online` until the reload budget runs out, or one
more ten seconds, whichever is longer. `Online` is a verdict shep gives once:
a replacement that passes its probe and dies moments later is respawned under
a new pid with the reload already committed, and a replacement whose second
probe fails is put back to `starting` and left there. The pid and the status
are the evidence of either, and only inside the dwell. An app that sets
`reuse_port` with a probe gets a longer one, its own `listen_timeout`, because
shep re-runs its probe after the old instance drains and can demote it any
time up to that.

The first cutover is also the one deploy that is not verified against the
readiness probe. shep reports a freshly started process `Online` once its
`listen_timeout` elapses, whatever the probe said, and only aborts a *reload*
whose replacement was not ready. So `setup` checks what it can: a new process
started and was still the same process, not errored and not restarted, ten
seconds later. A release that starts, stays up and serves nothing passes that.
Every deploy after the first is verified against a new process reaching
`Online`.

**That used to be weaker than it sounds, and the gap was shep's rather than
this crate's.** Measured 2026-08-28 against a real shepherd: a sheep with an
HTTP `readiness_probe` that never passes was marked `Online` about a second
into a reload, well inside its `listen_timeout`, and stayed there. The cause
was the reload's overlap. Both instances were up when the replacement's first
probe landed, the outgoing one answered it, and shep took that as the incoming
one proving itself. A release that started and never became ready was verified,
recorded as deployed, and not rolled back, with the app down while the record
said otherwise.

shep fixed it the same day. A probed app is now reloaded serially: the old
instance drains first, so the only process that can answer the replacement's
probe is the replacement. An app that sets `reuse_port` keeps the overlap it
asks for and gets a second probe once the drained instance is gone. Either way
a replacement that never answers is left `starting` rather than `online`, which
is what this crate reads, so `verify = "probed"` now means what it says.

One thing to size correctly. A `reuse_port` app's reload costs one more
`listen_timeout` than it used to, for that second probe, and the budget in
`window.rs` is `listen_timeout + graceful_timeout + slack` per instance. For
that one combination the budget can expire mid-check and roll back a release
that was fine. A false rollback rather than a false success, which is the right
direction to fail, but it wants a wider budget.

Rollback works for a release that crashes and for one that starts without
serving. Both are caught.

The rollback is verified the same way. The shepherd accepting its reload is not
enough: a previous release whose binary is gone is dropped from the flock after
the reload is accepted. A rollback is reported as one only once the old release
is seen to come up, within the restored release's own `listen_timeout` and
`graceful_timeout` rather than the rejected one's; otherwise nothing is known
to be serving, and the error says so and points at `shep flock` and `shep
describe <sheep>`.

## Removing it

`shep-deploy on-remove` is the lifecycle hook: shep runs it before forgetting
the dog, and it puts every sheep back as it ran before the dog took over: the
whole definition `setup` recorded, `env`, instances and probes included. A
record written before 2026-09-04 carries only the old `cwd` and script, and
those go back over the shepherd's current definition. A
sheep the dog bootstrapped has nowhere to go back to, so it is left running
from `current` and the report says exactly that, with the path.

The deploy tree is never deleted. It is not the dog's to delete, and in the
bootstrap case a running app is still pointing into it.

## Exit codes

Follows [shep's own
taxonomy](https://github.com/shep-pm/shep/blob/main/docs/specs/shep-v1.md):
`0` deployed or already up to date, `2` bad arguments, `4` bad configuration,
`5` no daemon answered or one refused this dog's handshake, `1` anything else.
`1` includes a rollback that did not come up: the previous release was put back
on disk, and nothing is known to be serving.

Two are this dog's own:

| code | means | the flock is |
|---|---|---|
| `12` | the deploy was rejected, the previous release was put back and came up | healthy, on the old release |
| `13` | a first cutover landed and then could not tidy up | healthy, on the new release |

A script that treats any nonzero code as "the deploy broke" will be wrong about
both, because in each case the flock is serving.

## Security

A deploy runs the build command from the repository being deployed. That is
the point of it, and it is also the whole of the risk: `bun install`'s
postinstall scripts and `make build` are arbitrary code, chosen by whoever can
land a commit on the branch you track.

That build runs as this process's uid unless the app sets `user`. Supervised as
a dog, this process is the shepherd's child and shares its uid, and shep's own
docs recommend running the shepherd as root so it can drop privileges per app.
So the default arrangement is a repository's build script running as root, once
per deploy.

A committed Flockfile is refused for setting anything the shepherd acts on at
its own uid: `user` and `group`, an `exec` probe (the daemon runs it through
`sh -c`, forever, ignoring `user`), `out_file` and `err_file` (the daemon
opens and `shep flush` truncates them), an `http` or `tcp` probe whose target
is not loopback (the daemon makes the connection), and `watch` (the daemon
walks the working directory as itself, following symlinks). It is also refused
for carrying `[dog.deploy.release]` or `[dog.deploy.after]`, because a hook's
`env` holds your secrets and a commit must not choose the command that receives
them. Every one of them stays available in `Flockfile.override.toml`, which is
yours.

Set `user` on the app and the build drops to that user's uid and primary group,
with the shepherd's supplementary groups cleared, before it runs anything. A
compromised build then gets that app's privileges and nothing more.

Under a root shepherd the build user owns its release and its build cache
directory (`target`, the directory only, not what is in it) and nothing else.
The dog hands those over just before the build, or before the release hook (or the
after hook) when the app has no build command, through open directory handles
and never through paths, and skips every symlink, every file with a second
hard link, and the `.git` file at the release root. The build can still replace
that file, since it owns the directory, so the dog writes it back after the
build. `git/`, `deploy.toml`, the lock files, `current` and `releases/`
stay root's. The user also needs search permission on every directory down to
the release: a `$SHEP_HOME` of mode 700 refuses the build at its first step.
Put `$SHEP_HOME` where that user can search, or run the shepherd as that user.

shep-deploy warns when it is about to run a build as root with no `user` set.
It does not refuse. Whether an app runs without a `user` is shep's call and the
operator's, not a deploy dog's.

A build starts from a cleared environment, not this process's. It gets `PATH`,
`HOME`, `LANG`, `LC_ALL` and `TZ`, whatever `passthrough` names in
`[deploy]` in `dogs.toml`, and the release's own `[dog.deploy.build] env`.
Nothing else. Dropping uid and gid bounds what a build can touch; it does
nothing about what it can read out of its own environment, because those values
are copied in before the drop happens. A dog started with a registry token in
its environment would otherwise hand it to every build it runs.

`SSH_AUTH_SOCK` is not in that base set, on purpose. A forwarded agent reaching
a build lets the build authenticate as you anywhere that agent is trusted.
Fetching happens in the dog's own process and keeps its own environment, so a
private repository still clones; only the build loses the socket. Name it in
`passthrough` if a build genuinely needs it.

The release's own `Flockfile.toml` is read in the dog's process too, before
any build drops to `user`, so a committed one that is a symlink is refused
unread, and a parse error names a line number and never quotes the line. Your
`Flockfile.override.toml` is the one file that is followed through its link,
because this dog made that link.

`build.artifacts` may only name paths that really land inside the release or
the dog's own build cache, resolved rather than spelled. A `..`, an absolute
path, a committed symlink, or a `CARGO_TARGET_DIR` pointing elsewhere are all
refused. That is narrower than it was: a build whose output genuinely lands
outside both is no longer copyable, because the copy runs in the dog's own
process at its own uid, and the `[dog.deploy.build]` block naming the path
comes from the deployed repository rather than from you.

**What the allowlist does not cover, and cannot.** Your project's own secrets
are not in the dog's environment, they are in your repository's working tree. A
`.env` file is gitignored, which is exactly the rule that makes shep-deploy
symlink it into every release, so the build reads it and so does the app at
runtime. That is the intended behaviour and the reason shared files exist. It
does mean a build command can read every secret the app itself can read, no
matter what this allowlist says. The bound worth having there is `user`, so
that a build and the app it builds are confined to the same one account.

Nothing else here handles credentials. Git auth is inherited from the user the
dog runs as, so a private repository works as it does in that user's own shell,
and no token passes through any URL or argument this crate builds.

## Platform

Unix only. This is deliberate, not a gap waiting to be filled by accident: the
deploy model is `rename(2)` over a symlink, the build's privilege drop is a uid
and a gid, and both are Unix concepts the code uses directly rather than
through a portability layer. Building on Windows fails with one sentence saying
so. Windows support is planned and will be scoped on its own.

## Status

Working: the deploy sequence, the operator commands, opt-in, the poll loop,
retention, and restore on removal. Tested against a real shepherd.

Not built: Windows.

See
[docs/writing-plans/plans/2026-08-26-deploy-engine.md](docs/writing-plans/plans/2026-08-26-deploy-engine.md)
for what was built, and the [design
spec](docs/brainstorming/specs/2026-08-26-deploy-dog-design.md) for what it is
for.

## License

MIT OR Apache-2.0, at your option.
