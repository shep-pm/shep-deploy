//! What a committed Flockfile may not say: the fields the shepherd acts on at
//! its own uid, and one app named twice.

use toml::Value;

use super::{app_name, apps};
use crate::error::Error;
use crate::shared::printable;

/// The boundary this module enforces: refuses a committed Flockfile that
/// sets any field the DAEMON acts on at its own uid, on any app it declares.
///
/// `user` and `group` were the whole list until 2026-09-04, on the theory
/// that privilege was the one thing a build could not reach on its own. A
/// read of shep-daemon found three more fields the daemon itself acts on,
/// without ever dropping privilege, so a committed value reaches the
/// shepherd's uid, which shep's own docs recommend be root:
///
/// - A `readiness_probe` or `liveness_probe` of kind `exec` is run by the
///   daemon through `sh -c`, on the probe's interval (ten seconds by
///   default), forever, with no uid drop at all. It ignores `user` even
///   when the app sets one. A committed one is a root shell on a timer.
/// - `out_file` and `err_file` are opened, created and appended to by the
///   daemon's own log pump, and `shep flush` truncates them, at the
///   daemon's uid. shep guards the final path component against a symlink
///   and refuses a loosely owned ancestry, and nothing else: a committed
///   `out_file = "/etc/cron.d/shep"` is appended to with whatever the app
///   prints, and emptied on `shep flush`.
/// - An `http` or `tcp` probe is connected to from the daemon's own network
///   position. Only the verdict leaks, so it is a blind primitive rather
///   than a read, and a committed target on loopback is the ordinary shape
///   of a health check; one anywhere else is refused.
/// - `watch` has the daemon watch the app's working directory recursively,
///   as itself, following symlinks (notify's default). A committed link out
///   of the release has a root shepherd walk and watch whatever it points
///   at, up to the whole host, and restart the app on any change there.
///
/// Takes the *committed* document specifically, never the merged one. If
/// this ran against the merged document instead, a legitimate use of the
/// mechanism - an operator pinning `user` or a log path in their own
/// override - would look identical to the thing being refused, since the
/// merged document would carry the key either way. Checking the committed
/// document alone, before the override is even read, keeps the two apart:
/// the committed file can never carry these, full stop, and the override
/// remains free to set them precisely because it is the operator's own file
/// and never came from the repo.
///
/// What this does NOT do is refuse a committed Flockfile that OMITS `user`.
/// That app runs at the daemon's uid, and so does its build; `build::run`
/// warns about it and does not refuse, which is Rin's call: whether an app
/// runs without a `user` is shep's and the operator's, not a deploy dog's.
///
/// # Errors
/// [`Error::Config`] naming the field and the app, for the first app in
/// `committed` that sets one of them.
pub(super) fn refuse_repo_privilege(committed: &Value) -> Result<(), Error> {
    for app in apps(committed) {
        let Some(table) = app.as_table() else {
            continue;
        };
        let name = table
            .get("name")
            .and_then(Value::as_str)
            .map(printable)
            .unwrap_or_else(|| "<unnamed>".to_owned());
        let refuse = |field: &str, why: &str| {
            Error::Config(format!(
                "Flockfile.toml sets `{field}` on app {name:?}: {why}. A committed Flockfile \
                 can never set it, on any app, because the shepherd acts on it at its own uid \
                 and a build cannot reach that on its own. Pin `{field}` in \
                 Flockfile.override.toml instead - it is gitignored and never comes from the \
                 repo."
            ))
        };

        for field in ["user", "group"] {
            if table.contains_key(field) {
                return Err(refuse(
                    field,
                    "privilege is the one thing a compromised build cannot escalate to",
                ));
            }
        }
        for field in ["out_file", "err_file"] {
            if table.contains_key(field) {
                return Err(refuse(
                    field,
                    "the shepherd opens, appends to and on `shep flush` truncates that path \
                     itself, at its own uid, with no check on where it points",
                ));
            }
        }
        if table.contains_key("watch") {
            return Err(refuse(
                "watch",
                "the shepherd watches the app's whole working directory itself, at its own \
                 uid, following symlinks, so a committed link out of the release has it walk \
                 and watch whatever the link points at",
            ));
        }
        for field in ["readiness_probe", "liveness_probe"] {
            // Presence first, shape second. serde deserialises a struct from a
            // sequence as well as a table, so `readiness_probe = ["exec", "cmd"]`
            // is a valid probe to shep, and a walk that only looked at tables
            // let it through untouched. Measured 2026-09-04.
            let Some(value) = table.get(field) else {
                continue;
            };
            let Some(probe) = value.as_table() else {
                return Err(refuse(
                    field,
                    "a probe the committed file spells as anything but a table is one this \
                     check cannot read, and shep would accept it",
                ));
            };
            let (Some(kind), Some(target)) = (
                probe.get("kind").and_then(Value::as_str),
                probe.get("target").and_then(Value::as_str),
            ) else {
                return Err(refuse(
                    field,
                    "a probe whose `kind` or `target` is not a string is one this check \
                     cannot read",
                ));
            };
            if kind == "exec" {
                return Err(refuse(
                    field,
                    "an exec probe is run by the shepherd through `sh -c` on the probe's \
                     interval, forever, at the shepherd's own uid and ignoring `user`",
                ));
            }
            if !probes_loopback(kind, target) {
                return Err(refuse(
                    field,
                    "the shepherd connects to the probe's target from its own network \
                     position, so a committed target has to be on loopback: `127.0.0.1`, \
                     `::1` or `localhost`",
                ));
            }
        }
    }
    Ok(())
}

/// Whether an `http` or `tcp` probe `target` names this host.
///
/// `http` targets are URLs and `tcp` targets are `host:port`; either way
/// the host is what matters, and the daemon is the one connecting to it.
/// Loopback by name or number, IPv4 or IPv6, with or without a port or a
/// path. Anything the parser does not recognise is not loopback: the
/// refusal costs an operator one line in their override, and a miss here
/// costs a blind connection from the shepherd to wherever the commit said.
fn probes_loopback(kind: &str, target: &str) -> bool {
    let host_and_rest = match kind {
        "http" => target
            .split_once("://")
            .map_or(target, |(_, rest)| rest)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or(""),
        "tcp" => target,
        _ => return false,
    };
    // Strip credentials, then a port. An IPv6 literal is bracketed.
    let host = host_and_rest.rsplit('@').next().unwrap_or("");
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                h
            } else {
                host
            }
        })
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.starts_with("127.")
            && host.split('.').count() == 4
            && host.split('.').all(|octet| octet.parse::<u8>().is_ok())
}

/// Refuses a committed Flockfile that declares one app name twice.
///
/// Two entries named `web` are two answers to "which one is the sheep", and
/// every reader here has to pick the same one. `select_app` takes the first;
/// `merge_apps` used to index by name with the last winning, so the
/// operator's override, `user` pin included, was merged onto an entry
/// nobody then read. Refusing the shape is simpler than making every reader
/// agree, and shep itself would not accept the file either.
///
/// # Errors
/// [`Error::Config`] naming the app, if any name appears more than once.
pub(super) fn refuse_duplicate_apps(committed: &Value) -> Result<(), Error> {
    let mut seen = std::collections::BTreeSet::new();
    for app in apps(committed) {
        if let Some(name) = app_name(app)
            && !seen.insert(name)
        {
            return Err(Error::Config(format!(
                "Flockfile.toml declares an app named {:?} more than once; one entry per name",
                printable(name)
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::flockfile::app_config;

    /// fails if a committed Flockfile can hand the shepherd a command to run
    /// as itself. An `exec` probe is run by the daemon through `sh -c`, on
    /// its interval, with no uid drop and no regard for `user`: a root shell
    /// every ten seconds. Read out of shep-daemon on 2026-09-04.
    #[test]
    fn a_committed_exec_probe_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n[app.readiness_probe]\nkind = \
             \"exec\"\ntarget = \"curl -fs http://127.0.0.1/health\"\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("an exec probe");
        let shown = err.to_string();
        assert!(
            shown.contains("readiness_probe") && shown.contains("sh -c"),
            "{shown}"
        );
    }

    /// fails if a probe spelled as a sequence walks past the check. serde
    /// builds a struct from `["exec", "cmd"]` as readily as from a table,
    /// and a walk that only read tables let exactly that through, with the
    /// `exec` refusal and the loopback rule both defeated. Measured
    /// 2026-09-04: `app_config` returned a `ProbeConfig { kind: Exec, .. }`.
    #[test]
    fn a_committed_probe_spelled_as_a_sequence_is_refused() {
        for probe in [
            "readiness_probe = [\"exec\", \"id > /tmp/pwn\"]",
            "readiness_probe = [\"http\", \"http://metadata.internal/\"]",
            "liveness_probe = \"exec\"",
        ] {
            let rel = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                rel.path().join("Flockfile.toml"),
                format!("[[app]]\nname = \"web\"\nscript = \"./run\"\n{probe}\n"),
            )
            .expect("committed");

            let err = app_config(rel.path(), "web", &[]).expect_err(probe);
            assert!(err.to_string().contains("_probe"), "{probe}: {err}");
        }
    }

    /// fails if a committed `watch` reaches the shepherd. It watches the
    /// working directory as itself, following symlinks, so a committed link
    /// out of the release has it walk whatever the link points at.
    #[test]
    fn a_committed_watch_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\nwatch = true\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("watch");
        assert!(err.to_string().contains("`watch`"), "{err}");
    }

    /// fails if a committed Flockfile can point the shepherd's log pump at a
    /// path of its choosing. The daemon opens and appends to `out_file` at
    /// its own uid and `shep flush` truncates it, with no check on where it
    /// points.
    #[test]
    fn a_committed_log_path_is_refused() {
        for field in ["out_file", "err_file"] {
            let rel = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                rel.path().join("Flockfile.toml"),
                format!(
                    "[[app]]\nname = \"web\"\nscript = \"./run\"\n{field} = \"/etc/cron.d/x\"\n"
                ),
            )
            .expect("committed");

            let err = app_config(rel.path(), "web", &[]).expect_err(field);
            let shown = err.to_string();
            assert!(
                shown.contains(field) && shown.contains("truncates"),
                "{shown}"
            );
        }
    }

    /// fails if a committed probe can send the shepherd's connection
    /// anywhere but this host, or if a loopback one is refused. A health
    /// check of the app itself is on loopback; the cloud metadata address is the
    /// cloud metadata service, reached from the daemon's network position.
    #[test]
    fn a_committed_network_probe_must_target_loopback() {
        let refused = [
            ("http", "http://metadata.internal/latest/meta-data"),
            ("http", "http://internal.example:8080/"),
            ("tcp", "192.0.2.5:5432"),
            ("tcp", "db:5432"),
            ("http", "http://127.0.0.1.evil.example/"),
            ("http", "http://localhost@evil.example/"),
        ];
        for (kind, target) in refused {
            assert!(
                !probes_loopback(kind, target),
                "{kind} {target} must be refused"
            );
        }
        let allowed = [
            ("http", "http://127.0.0.1:3000/health"),
            ("http", "http://localhost/health?deep=1"),
            ("http", "https://[::1]:8443/"),
            ("http", "http://user:pw@127.0.0.1/"),
            ("tcp", "127.0.0.1:5432"),
            ("tcp", "localhost:6379"),
            ("tcp", "[::1]:80"),
        ];
        for (kind, target) in allowed {
            assert!(
                probes_loopback(kind, target),
                "{kind} {target} must be allowed"
            );
        }

        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n[app.readiness_probe]\nkind = \
             \"http\"\ntarget = \"http://metadata.internal/\"\n",
        )
        .expect("committed");
        let err = app_config(rel.path(), "web", &[]).expect_err("off-host target");
        assert!(err.to_string().contains("loopback"), "{err}");
    }

    /// fails if a committed Flockfile naming one app twice is accepted.
    ///
    /// Found in round two of the review: the indexed merge kept the LAST
    /// entry for a repeated name while `select_app` read the first, so the
    /// operator's override, `user` pin included, landed on an entry nobody
    /// read and the sheep started at the dog's own uid. Refused by name now,
    /// and the index keeps the first regardless.
    #[test]
    fn a_committed_flockfile_naming_an_app_twice_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./a\"\n[[app]]\nname = \"web\"\nscript = \"./b\"\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("two apps named web");
        let shown = err.to_string();
        assert!(
            shown.contains("web") && shown.contains("more than once"),
            "{shown}"
        );
    }

    /// fails if a committed override can grant a unix user.
    ///
    /// The override is exempt from `refuse_repo_privilege` because it is the
    /// operator's file. Nothing checked that it was, and the exemption is
    /// worthless without the check: measured 2026-08-28, a repository that
    /// simply committed `Flockfile.override.toml` had `user = "root"`
    /// honoured, straight past the boundary this module's own doc calls the
    /// one real one.
    ///
    /// `link_into` refuses when something is already in the way, so a
    /// committed file cannot displace an override the operator does share.
    /// This is the case it cannot cover: an operator with no override at all,
    /// where nothing collides and the committed file simply arrives.
    #[test]
    fn a_committed_override_cannot_grant_a_user() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::fs::write(
            rel.path().join("Flockfile.override.toml"),
            "[[app]]\nname = \"web\"\nuser = \"root\"\n",
        )
        .expect("committed override");

        let err = app_config(rel.path(), "web", &[])
            .expect_err("a committed override must not grant a user");
        assert!(
            format!("{err}").contains("user"),
            "the refusal must name the field: {err}"
        );
    }

    /// fails if a committed Flockfile can set the user a process runs as.
    /// Privilege is not a recommendation, and it is the one thing a
    /// compromised build genuinely cannot escalate on its own. This refusal
    /// is the boundary.
    #[test]
    fn a_committed_flockfile_cannot_set_user() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\nuser='root'\n",
        )]);
        let err = app_config(rel.path(), "web", &[]).expect_err("refuses");
        assert!(err.to_string().contains("user"));
    }

    /// fails if `group` is not checked as its own clause. `user` and `group`
    /// are two separate `contains_key` checks in `refuse_repo_privilege`,
    /// not one - the test above only proves the first field is guarded, and
    /// a version of this function that checked `user` and forgot `group`
    /// would still pass it.
    #[test]
    fn a_committed_flockfile_cannot_set_group() {
        let rel = fixtures::fixture_release(&[(
            "Flockfile.toml",
            "[[app]]\nname='web'\nscript='x.js'\ngroup='wheel'\n",
        )]);
        let err = app_config(rel.path(), "web", &[]).expect_err("refuses");
        assert!(err.to_string().contains("group"));
    }
}
