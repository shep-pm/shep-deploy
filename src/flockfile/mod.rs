//! Reading the release's own Flockfile, merging the operator's override over
//! it, and producing the [`AppConfig`] for one sheep.
//!
//! Two files, and it matters which of them is trusted:
//!
//! - `Flockfile.toml`, committed in the release. Upstream owns it - it is
//!   whatever the tracked branch happened to contain at this release's sha.
//! - `Flockfile.override.toml`, gitignored, never committed. The operator
//!   owns it, and it deep merges over the committed file with the override
//!   winning on every key it names. A sheep with no override simply follows
//!   upstream on every deploy; a sheep whose operator pins `script` or `cwd`
//!   in an override keeps that value even when a pull changes upstream's
//!   own copy.
//!
//! ## The one real boundary here
//!
//! Everything above is defence in depth, not a boundary: a compromised
//! upstream already runs arbitrary code the moment this dog runs `bun
//! install`'s postinstall or `make build`, and pinning `script` in an
//! override does not stop that - it only stops a *later* pull from moving
//! the pointer. What a build cannot reach on its own is the SHEPHERD's uid,
//! and a handful of Flockfile fields make the shepherd act at it: `user`
//! and `group` choose the uid an app runs as, an `exec` probe is a command
//! the shepherd runs itself, `out_file` and `err_file` are paths the
//! shepherd opens and truncates itself, and an `http` or `tcp` probe is a
//! connection the shepherd makes itself. So a *committed* Flockfile is
//! refused outright the moment it sets any of those on any app it declares
//! (a loopback probe target excepted) - see [`refuse::refuse_repo_privilege`] - and
//! the check runs against the committed document alone, before the
//! override is even read, so an override cannot make the refusal disappear
//! by also touching the same key. The override itself is under no such
//! restriction: it is the operator's own file, and pinning `user` or a log
//! path there is exactly the "pin it so upstream can never change it"
//! mechanism every other field already gets.

mod read;
mod refuse;

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use shep_client::shep_core::prelude::AppConfig;
use toml::{Table, Value};

use crate::error::Error;
use crate::shared::{FromCheckout, printable};

pub(crate) use read::OVERRIDE;
use read::{Follow, read_optional, read_required};
use refuse::{refuse_duplicate_apps, refuse_repo_privilege};

/// The two Flockfiles of one release, read once and merged.
///
/// Reading through this type parses once and answers every question from the
/// same document, which is also the only way two answers cannot disagree.
#[derive(Debug)]
pub struct Merged {
    /// The committed document with the override merged over it.
    doc: Value,
}

/// Reads and merges `release`'s Flockfiles, refusing first if the committed
/// one sets anything the shepherd acts on at its own uid.
///
/// # Errors
/// [`Error::Io`], naming the failing path, if `Flockfile.toml` cannot be
/// read or `Flockfile.override.toml` exists but cannot be read.
/// [`Error::Config`] if either file is not valid TOML, is a symlink the
/// repository committed, or is past `MAX_FLOCKFILE_BYTES`; if the
/// committed file names one app twice; or if it sets, on any app, `user`,
/// `group`, `out_file`, `err_file`, `watch`, an `exec` probe, a probe whose
/// target is off loopback, or a probe spelled as anything but a table. See
/// [`refuse_repo_privilege`] for why each is on the list.
pub fn read(release: &Path, shared: &FromCheckout) -> Result<Merged, Error> {
    Ok(Merged {
        doc: merged_document(release, shared)?,
    })
}

impl Merged {
    /// The [`AppConfig`] for `sheep`: the merged entry named `sheep`,
    /// deserialised.
    ///
    /// # Errors
    /// [`Error::Config`] if no app named `sheep` exists after merging, or if
    /// that app's table does not match [`AppConfig`]'s schema.
    pub fn app_config(&self, sheep: &str) -> Result<AppConfig, Error> {
        let app = select_app(&self.doc, sheep)?;
        app.try_into().map_err(|source: toml::de::Error| {
            Error::Config(format!(
                "app {sheep:?} does not match shep's app schema: {}",
                printable(source.message())
            ))
        })
    }
}

/// The [`AppConfig`] for `sheep`, built from `release`'s own Flockfile.
///
/// Reads `Flockfile.toml` (required) and `Flockfile.override.toml`
/// (optional - a sheep with none simply has nothing to merge), refuses if
/// the committed file sets `user` or `group` on any app, deep merges the
/// override over the committed document with arrays of apps matched by
/// `name` rather than position, then selects and deserialises the entry
/// named `sheep`.
///
/// # Errors
/// [`Error::Io`], naming the failing path, if `Flockfile.toml` cannot be
/// read or `Flockfile.override.toml` exists but cannot be read.
/// [`Error::Config`] if either file is not valid TOML, if the committed
/// file sets `user` or `group` on any app, if no app named `sheep` exists
/// after merging, or if that app's table does not match [`AppConfig`]'s
/// schema.
///
/// The deploy and the cutover read through [`read()`] and ask both questions
/// of one document; this one-question form is what the tests drive.
#[cfg(test)]
pub fn app_config(release: &Path, sheep: &str, shared: &[PathBuf]) -> Result<AppConfig, Error> {
    read(release, &FromCheckout::of(shared.to_vec()))?.app_config(sheep)
}

/// The committed Flockfile with the operator's override merged over it,
/// refusing first if the committed file sets anything the shepherd acts on
/// at its own uid (see [`refuse_repo_privilege`]).
///
/// Both public readers go through here, so the refusal applies whichever
/// one was called and neither can see a document the other could not.
///
/// # Errors
/// As [`read()`].
fn merged_document(release: &Path, shared: &FromCheckout) -> Result<Value, Error> {
    // The committed file is never followed through a symlink: it is the
    // repository's, and the repository does not get to choose which file on
    // this host the dog reads. See `read_flockfile`.
    let committed = read_required(&release.join("Flockfile.toml"), Follow::Never)?;
    refuse_repo_privilege(&committed)?;
    refuse_duplicate_apps(&committed)?;

    // The override is exempt from the privilege refusal only because it is the
    // OPERATOR's file, and that has to be established rather than assumed. A
    // repository that simply committed a `Flockfile.override.toml` had
    // `user = "root"` honoured, which is the one thing this module's own doc
    // calls the real boundary. Measured 2026-08-28.
    //
    // Asked of `shared`, the list of paths the caller just linked in from the
    // operator's checkout, because that is the only record of where a file
    // came from. Only `shared::to_link` can build that list, which is what
    // makes the answer evidence rather than a claim; its doc records the
    // filesystem-based versions of this check that came before, and why
    // none of them could work.
    //
    // Decided BEFORE the read, because it also decides whether the read may
    // follow a symlink: the operator's override IS one, into their checkout,
    // and a committed one must not be.
    let operators = shared.includes_override();
    let override_path = release.join(OVERRIDE);
    let override_doc = read_optional(
        &override_path,
        if operators {
            Follow::Operators
        } else {
            Follow::Never
        },
    )?;
    if !operators {
        refuse_repo_privilege(&override_doc)?;
    }

    Ok(deep_merge(committed, override_doc))
}

/// `doc`'s `[[app]]` array, or an empty slice if `doc` is not a table or has
/// no `app` key at all - an empty Flockfile has no apps to iterate rather
/// than being a shape error at this layer.
fn apps(doc: &Value) -> &[Value] {
    doc.as_table()
        .and_then(|table| table.get("app"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Deep merges `over` onto `base`, `over` winning on every key it names.
///
/// Two tables merge key by key, recursing into any key both sides declare
/// as a table so a partial override only replaces the fields it actually
/// names - an override setting only `script` leaves every other field of
/// that app, and every other app, untouched. The `app` key is special-cased
/// to [`merge_apps`] rather than falling into that same table-of-tables
/// recursion, because it is not a table keyed by name - it is an array, and
/// the two Flockfiles are under no obligation to list their apps in the
/// same order. Anything else - two scalars, or a table on one side and a
/// scalar on the other - has `over` win outright, which is the base case
/// every recursive branch above eventually bottoms out at.
fn deep_merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Table(mut base), Value::Table(over)) => {
            for (key, over_value) in over {
                let merged = if key == "app" {
                    merge_apps(
                        base.remove(&key).unwrap_or(Value::Array(Vec::new())),
                        over_value,
                    )
                } else {
                    match base.remove(&key) {
                        Some(base_value) => deep_merge(base_value, over_value),
                        None => over_value,
                    }
                };
                base.insert(key, merged);
            }
            Value::Table(base)
        }
        (_, over) => over,
    }
}

/// Merges two `[[app]]` arrays by matching each entry's `name`, not its
/// position.
///
/// An override entry whose `name` matches a committed entry deep-merges
/// onto it in place, at the committed entry's own position, so a partial
/// override still leaves every field it does not name untouched - the same
/// property [`deep_merge`] gives every other table in the document. An
/// override entry whose `name` matches nothing in `base` is appended as a
/// new app. Position carries no meaning: the committed file and the
/// override are free to list the same apps in different orders and still
/// merge onto the same entries.
fn merge_apps(base: Value, over: Value) -> Value {
    let mut base_apps = match base {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };
    let over_apps = match over {
        Value::Array(apps) => apps,
        _ => Vec::new(),
    };

    // Indexed once rather than scanned per override entry. Both arrays come
    // from the repository, and a scan per entry made the merge quadratic in
    // files nothing else bounds the length of.
    //
    // First wins on a repeated name, the way `select_app` reads the array.
    // `collect` into a map keeps the LAST, which put an override onto an
    // entry nothing then read; `refuse_duplicate_apps` stops the shape at the
    // door, and this keeps the two readers agreeing even so.
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    for (index, app) in base_apps.iter().enumerate() {
        if let Some(name) = app_name(app) {
            by_name.entry(name.to_owned()).or_insert(index);
        }
    }

    for over_app in over_apps {
        let existing = app_name(&over_app).and_then(|name| by_name.get(name).copied());

        match existing {
            Some(index) => {
                let base_app = std::mem::replace(&mut base_apps[index], Value::Table(Table::new()));
                base_apps[index] = deep_merge(base_app, over_app);
            }
            None => {
                if let Some(name) = app_name(&over_app) {
                    by_name.entry(name.to_owned()).or_insert(base_apps.len());
                }
                base_apps.push(over_app);
            }
        }
    }

    Value::Array(base_apps)
}

/// `app`'s `name` field, if it has one and it is a string.
fn app_name(app: &Value) -> Option<&str> {
    app.as_table()?.get("name")?.as_str()
}

/// The merged document's app entry named `sheep`.
///
/// # Errors
/// [`Error::Config`] if no app in `merged` is named `sheep`.
fn select_app(merged: &Value, sheep: &str) -> Result<Value, Error> {
    apps(merged)
        .iter()
        .find(|app| app_name(app) == Some(sheep))
        .cloned()
        .ok_or_else(|| Error::Config(format!("no app named {sheep:?} in Flockfile.toml")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::fixtures;

    /// fails if the operator's own override loses any of the fields the
    /// committed file is refused. The override is the operator's, and
    /// these are exactly the things they pin there.
    #[test]
    fn the_operators_override_may_set_every_refused_field() {
        let rel = tempfile::tempdir().expect("tempdir");
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::fs::write(
            checkout.path().join(OVERRIDE),
            "[[app]]\nname = \"web\"\nuser = \"svc\"\nout_file = \"/var/log/web.log\"\n\
             [app.readiness_probe]\nkind = \"exec\"\ntarget = \"./healthy\"\n",
        )
        .expect("the operator's override");
        std::os::unix::fs::symlink(checkout.path().join(OVERRIDE), rel.path().join(OVERRIDE))
            .expect("linked in");

        let app = app_config(rel.path(), "web", &[PathBuf::from(OVERRIDE)])
            .expect("the operator may set all of them");
        assert_eq!(app.user.as_deref(), Some("svc"));
        assert_eq!(app.out_file.as_deref(), Some("/var/log/web.log"));
        assert!(app.readiness_probe.is_some());
    }

    /// fails if the operator's own override stops being able to pin a user.
    ///
    /// The counterpart. Theirs reaches a release as a symlink into their
    /// checkout, so it resolves outside the release and keeps the exemption,
    /// which is the whole "pin it so upstream cannot change it" mechanism.
    #[test]
    fn the_operators_own_override_still_pins_a_user() {
        let rel = tempfile::tempdir().expect("tempdir");
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        let theirs = checkout.path().join("Flockfile.override.toml");
        std::fs::write(&theirs, "[[app]]\nname = \"web\"\nuser = \"svc\"\n").expect("theirs");
        std::os::unix::fs::symlink(&theirs, rel.path().join("Flockfile.override.toml"))
            .expect("shared in");

        // The share list is what makes it theirs, exactly as `link_into`
        // would have reported it.
        let shared = [PathBuf::from("Flockfile.override.toml")];
        let app =
            app_config(rel.path(), "web", &shared).expect("the operator's override is honoured");
        assert_eq!(app.user.as_deref(), Some("svc"));
    }

    /// fails if the override stops winning. The override is the user's file
    /// and the committed one is upstream's; a user who pins script must not
    /// have it changed underneath them by a pull.
    #[test]
    fn the_override_wins_on_merge() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='upstream.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nscript='mine.js'\n",
            ),
        ]);
        let app = app_config(rel.path(), "web", &[]).expect("merges");
        assert_eq!(app.script, "mine.js");
    }

    /// fails if the presence of an override makes the committed-file
    /// refusal disappear. The refusal must fire because it was decided by
    /// reading the committed file alone, before the override was opened.
    ///
    /// The override deliberately does NOT name `user`. It did, set to
    /// `nobody`, on the theory that a laundered merge would be the shape
    /// worth catching. That made the test vacuous: `shared` is empty here, so
    /// `includes_override` is false, so `merged_document` runs
    /// `refuse_repo_privilege` against the override document too, and THAT
    /// check produced the error being asserted on. Deleting the
    /// committed-document check left the test passing. Found in round 8 of
    /// the founder's review, by deleting that line and re-running.
    ///
    /// `script` is the right field precisely because nothing refuses it, so
    /// the only remaining route to an error is the check this test is named
    /// for. `a_committed_flockfile_cannot_set_user` covers the other one.
    #[test]
    fn an_override_present_does_not_launder_a_committed_user_field() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='web'\nscript='mine.js'\n",
            ),
        ]);
        let err = app_config(rel.path(), "web", &[]).expect_err("still refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if apps stop merging by `name` and fall back to merging by
    /// position instead. The committed file and the override list the same
    /// two apps in opposite orders; a positional merge would splice `web`'s
    /// committed entry with `worker`'s override entry (and vice versa),
    /// producing a merged app with a mismatched name/script pairing instead
    /// of leaving each app's own fields untouched aside from what its own
    /// override actually names.
    #[test]
    fn apps_in_different_files_merge_by_name_not_position() {
        let rel = fixtures::fixture_release(&[
            (
                "Flockfile.toml",
                "[[app]]\nname='web'\nscript='web.js'\n\n[[app]]\nname='worker'\nscript='worker.js'\n",
            ),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='worker'\nscript='worker-mine.js'\n\n[[app]]\nname='web'\nscript='web-mine.js'\n",
            ),
        ]);
        let web = app_config(rel.path(), "web", &[]).expect("merges web");
        let worker = app_config(rel.path(), "worker", &[]).expect("merges worker");
        assert_eq!(web.script, "web-mine.js");
        assert_eq!(worker.script, "worker-mine.js");
    }

    /// fails if `merge_apps` stops appending an override app that names
    /// nothing in the committed file - the other half of "matched by name":
    /// a name match merges in place, and a name miss must still make the
    /// app available rather than being silently dropped.
    #[test]
    fn an_override_can_add_a_new_app_not_in_the_committed_file() {
        let rel = fixtures::fixture_release(&[
            ("Flockfile.toml", "[[app]]\nname='web'\nscript='web.js'\n"),
            (
                "Flockfile.override.toml",
                "[[app]]\nname='sidecar'\nscript='sidecar.js'\n",
            ),
        ]);
        let sidecar =
            app_config(rel.path(), "sidecar", &[]).expect("the override's own app merges");
        assert_eq!(sidecar.script, "sidecar.js");
    }

    /// fails if asking for an app nobody declared silently produces
    /// something instead of a named refusal.
    #[test]
    fn app_config_refuses_an_unknown_sheep_name() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\n",
        )]);
        let err = app_config(rel.path(), "ghost", &[]).expect_err("no such app");
        assert!(err.to_string().contains("ghost"));
    }
}
