//! Putting one sheep back: delete, start, and the fallback if start is refused.

use std::collections::BTreeMap;
use std::path::Path;

use shep_client::shep_core::config::AppConfig;

use crate::daemon::Daemon;
use crate::error::Error;
use crate::paths::Tree;

/// What happened to one sheep, distinguishing "nothing changed" from
/// "it is deleted", because those need different words in the report.
pub(super) enum PutBack {
    /// Re-registered at its own checkout.
    Done,
    /// Nothing happened: the restore was never attempted, or was refused
    /// before anything about the sheep changed.
    Untouched(Error),
    /// The restore was refused, but the fallback re-registered the config
    /// the shepherd already had. NOT the same claim as `Untouched`: the
    /// delete already ran, so the sheep was stopped and a fresh instance
    /// started to get back to that config.
    Reset(Error),
    /// At least one delete already landed before another one refused.
    /// NOT `Untouched` (at least one instance really is gone) and NOT
    /// `Deleted` either - whether the registered config survived depends
    /// on whether the deletes that landed included the last live instance,
    /// which this loop does not track.
    PartlyDeleted(Error),
    /// The delete landed and neither the restore nor the fallback did.
    Deleted(Error),
}

/// Re-registers one sheep as it was before this dog took over: the whole
/// definition when the record carries one, or the `cwd` and `script` it ran
/// with over the shepherd's current definition when it does not.
///
/// Delete THEN start, and the order is tested. `Request::Start` on an
/// already-registered name adds an instance rather than re-registering it,
/// so starting first would leave the sheep running from both places at
/// once, which is the same fact the cutover is built on. Here that order
/// also leaves a CLEAN roll, unlike the cutover's: the delete drops the
/// name, so the following `Start` re-records against a name with no stale
/// entry behind it.
///
/// # Why there is a fallback
///
/// `Delete` is stop plus deregister, and the roll drops a name with no live
/// instance, so a refused `Start` here leaves the sheep gone from the flock
/// AND the roll, not returning on a reboot. The fallback re-registers the
/// config the shepherd had a moment ago, which costs one request on the
/// transient failures that are the common case. Only when that fails too is
/// the sheep genuinely gone, and the caller says so in those words.
pub(super) async fn put_back<D: Daemon>(
    daemon: &D,
    sheep: &str,
    registered: &BTreeMap<String, AppConfig>,
    origin: Option<&AppConfig>,
    cwd: &Path,
    script: &str,
) -> PutBack {
    let Some(current) = registered.get(sheep).cloned() else {
        return PutBack::Untouched(Error::Config(format!(
            "{sheep} is no longer registered, so there is nothing to put back"
        )));
    };

    // The whole pre-adoption definition when the record has it, so `env`,
    // `instances`, the probes and the rest go back too. A record from before
    // that field carries only `cwd` and `script`, and those go over whatever
    // the shepherd has now, which is the deployed repository's definition.
    let restored = match origin {
        Some(origin) => {
            let mut restored = origin.clone();
            restored.name = sheep.to_owned();
            // `prepare` refuses a sheep with no cwd, so an origin without
            // one is a hand-edited record. The legacy field is the next
            // best witness, and using it here is what keeps the report,
            // which names the same field, truthful.
            restored
                .cwd
                .get_or_insert_with(|| cwd.display().to_string());
            restored
        }
        None => {
            let mut restored = current.clone();
            restored.cwd = Some(cwd.display().to_string());
            restored.script = script.to_owned();
            restored
        }
    };

    let live = match daemon.describe(sheep).await {
        Ok(live) => live,
        Err(err) => return PutBack::Untouched(err),
    };
    let mut any_delete_landed = false;
    for info in &live {
        if let Err(err) = daemon.delete(info.id).await {
            // The discriminator is whether a PRIOR delete already landed,
            // not whether this one failed: a refusal on the very first
            // instance with nothing behind it has changed nothing at all,
            // and is `Untouched` exactly as before this variant existed. A
            // refusal after at least one delete succeeded is the case
            // `Untouched` cannot describe truthfully - see
            // `PutBack::PartlyDeleted`'s doc.
            return if any_delete_landed {
                PutBack::PartlyDeleted(err)
            } else {
                PutBack::Untouched(err)
            };
        }
        any_delete_landed = true;
    }

    match daemon.start(vec![restored]).await {
        Ok(_) => PutBack::Done,
        Err(err) => {
            // The sheep is deregistered at this point. Put the shepherd's
            // own config back rather than leaving it deleted, because a
            // refused restore is usually a bad origin_script or a user that
            // no longer resolves, and the config that was working a moment
            // ago still is.
            if daemon.start(vec![current]).await.is_ok() {
                PutBack::Reset(err)
            } else {
                PutBack::Deleted(err)
            }
        }
    }
}

/// Whether the shepherd has `sheep` registered against `tree` rather than
/// against the operator's own checkout.
///
/// `crate::optin::cut_over` sets `cwd` to the tree's `current` symlink and
/// nothing else in this crate does, so this is the shepherd's own answer to
/// "did the cutover get as far as registering". A tree's record cannot answer
/// it: `deployed` is written on the verified path only.
pub(super) fn registered_against(
    registered: &BTreeMap<String, AppConfig>,
    sheep: &str,
    tree: &Tree,
) -> bool {
    registered
        .get(sheep)
        .and_then(|app| app.cwd.as_deref())
        // Two spellings of one directory compare equal, and a `current`
        // whose release is already gone still compares by its parent: see
        // `shared::resolved` for why a literal comparison here left a
        // sheep the cutover DID register running from the tree.
        .is_some_and(|cwd| crate::shared::same_path(Path::new(cwd), &tree.current()))
}
