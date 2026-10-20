use core::fmt;

use super::Error;
use crate::shared::printable;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "cannot reach the shepherd: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused a request: {err}"),
            Self::Refused {
                daemon_version,
                message,
            } => write!(
                f,
                "the shepherd refused this dog's handshake: {message}. It is running {}, and \
                 the two no longer speak the same protocol. Upgrade whichever of them is \
                 behind; the shepherd starts this dog again on its own once they agree",
                match daemon_version {
                    Some(version) => format!("shep {version}"),
                    None => "a version it did not name".to_owned(),
                }
            ),
            Self::Protocol(what) => write!(f, "unexpected answer from the shepherd: {what}"),
            Self::Config(what) => write!(f, "bad deploy configuration: {what}"),
            Self::Smit(err) => write!(f, "could not publish a smit: {err}"),
            Self::Held { sheep, sha } => write!(
                f,
                "{sheep} is held at {sha}: the last attempt at that commit did not land, so it \
                 is left alone until the branch moves rather than rebuilt and rolled back every \
                 interval. Push a fix, or run `shep deploy {sheep}` to try the same commit again"
            ),
            Self::NotCutOver { sheep, tree } => write!(
                f,
                "{sheep} has a deploy tree but was never cut over to it, so there is nothing to \
                 deploy: its record names no released sha, and nothing has ever been served from \
                 that tree. Deploying now would reload {sheep} at its own checkout and report \
                 success for a release nothing ran. Finish the cutover instead - remove {} and \
                 run `shep-deploy setup {sheep}`.",
                tree.display()
            ),
            // Through `printable`, because a path here can be one the deployed
            // repository chose (an artifact's), and git's stderr quotes ref
            // names the repository chose too.
            Self::Io { path, source } => {
                write!(f, "{}: {source}", printable(path.display()))
            }
            Self::Git {
                command,
                status,
                stderr,
            } => match status {
                Some(code) => write!(
                    f,
                    "`{command}` exited with status {code}: {}",
                    printable(stderr)
                ),
                None => write!(
                    f,
                    "`{command}` was killed by a signal: {}",
                    printable(stderr)
                ),
            },
            Self::Raced {
                sheep,
                started,
                found,
            } => write!(
                f,
                "another deploy of {sheep} moved current from {started} to {found} while this \
                 one was building - refusing to swap on top of it, because that release may be \
                 newer than this one"
            ),
            Self::RollbackFailed { why, source } => write!(
                f,
                "rolling back after {why} failed: {source} - the rollback can stop at any of its \
                 three steps, so check `current`, deploy.toml and what the shepherd is actually \
                 running against each other before retrying"
            ),
            Self::Split {
                sheep,
                on,
                running,
                why,
                source,
            } => write!(
                f,
                "{sheep}: {why}, so it was rolled back to {on} - but no reload onto it could be \
                 confirmed ({source}). current and deploy.toml both name {on}; one or more \
                 instances may still be running {running}. The reload that blocked this one has \
                 to finish first: a `stopping` row in `shep flock` means it is still draining, \
                 but the earlier part of a swap shows no row of its own, so wait for the pids in \
                 `shep describe {sheep}` to hold still. Then `shep reload {sheep}` finishes the \
                 rollback."
            ),
            Self::RolledBack { to, source } => {
                write!(f, "rolled back to {to} after: {source}")
            }
            Self::Unverified { sheep, sha, why } => write!(
                f,
                "{sheep}: {why}, and there is no earlier release to roll back to - neither \
                 current nor deploy.toml names one that is still on disk - so it is still \
                 pointed at {sha}"
            ),
            Self::RollbackUnverified {
                sheep,
                to,
                why,
                found,
            } => write!(
                f,
                "{sheep}: {why}, so it was rolled back to {to} - but the rollback could not be \
                 confirmed: {found}. current and deploy.toml both name {to} and the shepherd \
                 accepted the reload onto it, but nothing is known to be serving. Check \
                 `shep flock` for a {sheep} row and `shep describe {sheep}` for what it is \
                 doing"
            ),
            Self::CutOver {
                sheep,
                why,
                removed,
                repaired,
                tree,
                source,
            } => {
                write!(f, "{sheep}'s first cutover did not come up. {why}")?;
                if let Some(source) = source {
                    write!(f, " ({source})")?;
                }
                if *removed {
                    // Not "the instance it added has been removed": a `why`
                    // of "no new instance appeared" means none was ever
                    // added, and the two sentences then contradicted each
                    // other one after the other. This is true either way.
                    write!(
                        f,
                        " Nothing this cutover started is left registered, and the original is \
                         still running on the release it was already serving."
                    )?;
                } else {
                    // `undo_start` swallows each delete so the operator gets
                    // the reason they are here rather than a second error
                    // about the cleanup, which means this is the only place
                    // a failed one is ever mentioned.
                    //
                    // No cwd hint. Two different instances can fail to be
                    // deleted here and only one of them is under the deploy
                    // tree: the repair `Start` re-registers the ORIGINAL
                    // config, so the instance it spawns has the operator's
                    // own checkout as its cwd, identical to the survivor's.
                    // A hint that is wrong half the time sends an operator
                    // looking for something that is not there.
                    write!(
                        f,
                        " An instance this cutover started could NOT be removed, so {sheep} may \
                         now be running more instances than it was: `shep describe {sheep}` \
                         lists them with their ids, and `shep delete <id>` removes one. Compare \
                         the count against what {sheep} is configured to run."
                    )?;
                }
                if !repaired {
                    // The half an operator cannot see. `shep flock` shows a
                    // healthy sheep either way; only the persisted roll is
                    // wrong, and only a restart reveals it. Printed before
                    // the remove-and-setup paragraph below, and flagged as
                    // coming first: an operator working in printed order
                    // must repair the roll before removing the tree, or
                    // `shep-deploy setup` reads a still-poisoned record.
                    write!(
                        f,
                        " Before that, one thing is NOT back as it was: the shepherd recorded \
                         the new release against {sheep} when it accepted the start, and that \
                         record could not be put back. It is correct in the running process and \
                         wrong on disk, so a daemon restart followed by `shep muster` would bring \
                         {sheep} back on the release that was just rejected. Re-register it from \
                         its own Flockfile to correct the record before restarting the shepherd."
                    )?;
                }
                // The half that turns a failed cutover into a false green.
                // `deploy` does not short-circuit on a record naming no
                // release, so it would build, swap, reload the sheep at its
                // own checkout, see a real pid turnover and print success.
                write!(
                    f,
                    " {sheep} is NOT a deploy target: its record names no deployed release and \
                     nothing has ever been served from its tree. Do NOT run `shep deploy \
                     {sheep}` against it - that would reload the sheep at its own checkout and \
                     report success for a release nothing served. Fix what this message names, \
                     remove {}, and run `shep-deploy setup {sheep}` again.",
                    tree.display()
                )?;
                Ok(())
            }
            Self::Stranded { sheep, sha, ids } => {
                let removes: Vec<String> =
                    ids.iter().map(|id| format!("`shep delete {id}`")).collect();
                write!(
                    f,
                    "{sheep} was cut over to {sha} and the new release is serving, but {} of the \
                     instances it replaced could not be removed and are still registered. They \
                     are still running the code from {sheep}'s own checkout, and they will not \
                     go away on their own: every deploy from now on reloads EVERY instance of \
                     the name and respawns each from its own spec, so these come back on the \
                     pre-adoption config each time. Remove them: {}.",
                    ids.len(),
                    removes.join(", ")
                )
            }
            Self::AlreadyDeploying { sheep } => write!(
                f,
                "another deploy of {sheep} is already running; this one did nothing. \
                 The dog polls on its own, so a deploy started by hand while a tick \
                 is in flight meets this"
            ),
            Self::BuildTimedOut { after } => write!(
                f,
                "the build did not finish within {after:?} and was abandoned; raise \
                 build_timeout under [deploy] in dogs.toml if it legitimately needs longer"
            ),
            Self::Build { status } => match status {
                Some(code) => write!(f, "the build exited with status {code}"),
                None => write!(f, "the build was killed by a signal"),
            },
            Self::Panicked { sheep, what } => write!(
                f,
                "the deploy of {sheep} panicked partway: {what}. That is a bug in shep-deploy \
                 rather than in {sheep}. Compare `current`, deploy.toml and `shep describe \
                 {sheep}` against each other before deploying again, and please report the \
                 panic"
            ),
        }
    }
}
