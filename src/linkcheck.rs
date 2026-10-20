//! Linked paths that look like build output.
//!
//! A deploy links every gitignored path the operator's `.shepignore` does not
//! name, so a build directory or a declared artifact in the checkout is shared
//! into every release. This module only names them. `target` is not in the
//! list: [`crate::shared::link_cache`] claims it first.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::shared::{FromCheckout, printable};

const BUILD_DIRECTORIES: [&str; 4] = ["node_modules", ".output", "dist", ".next"];

/// One line per linked path that looks like build output, each saying why and
/// how to stop linking it. Empty when nothing does.
///
/// A path is named when a whole component of it is a build directory name, or
/// when it and a `build.artifacts` entry are equal or one holds the other.
/// Names the first reason found, so a path is never named twice.
pub(crate) fn warnings(sheep: &str, shared: &FromCheckout, artifacts: &[PathBuf]) -> Vec<String> {
    shared
        .iter()
        .filter_map(|path| {
            let reason = reason(path, artifacts)?;
            Some(format!(
                "shep-deploy: {sheep}: warning: `{}` is linked from the checkout into every \
                 release, and {reason}. Add it to `.shepignore` in the checkout to stop linking it.",
                printable(path.display()),
            ))
        })
        .collect()
}

fn reason(path: &Path, artifacts: &[PathBuf]) -> Option<String> {
    let names = names(path);
    if let Some(name) = names
        .iter()
        .find_map(|name| BUILD_DIRECTORIES.iter().find(|build| *name == **build))
    {
        return Some(format!(
            "it has a `{name}` component, which holds build output"
        ));
    }
    artifacts.iter().find_map(|artifact| {
        let declared = self::names(artifact);
        (!declared.is_empty() && (names.starts_with(&declared) || declared.starts_with(&names)))
            .then(|| {
                format!(
                    "it overlaps `build.artifacts` entry `{}`, which the build writes",
                    printable(artifact.display())
                )
            })
    })
}

/// The named components of `path`, so `./out` and `out` compare equal.
fn names(path: &Path) -> Vec<&OsStr> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked(paths: &[&str]) -> FromCheckout {
        FromCheckout::of(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn a_linked_build_directory_is_named() {
        for name in ["node_modules", ".output", "dist", ".next"] {
            for path in [name.to_string(), format!("packages/app/{name}")] {
                let found = warnings("web", &linked(&[&path]), &[]);
                assert_eq!(found.len(), 1, "{path}: {found:?}");
                assert!(found[0].starts_with("shep-deploy: web:"), "{found:?}");
                assert!(found[0].contains(&path), "{found:?}");
                assert!(found[0].contains(".shepignore"), "{found:?}");
            }
        }
    }

    #[test]
    fn a_linked_path_that_overlaps_an_artifact_is_named() {
        let artifacts = [PathBuf::from("out/bin/koji")];
        for path in ["out/bin/koji", "out", "out/bin/koji/debug"] {
            let found = warnings("web", &linked(&[path]), &artifacts);
            assert_eq!(found.len(), 1, "{path}: {found:?}");
            assert!(found[0].starts_with("shep-deploy: web:"), "{found:?}");
            assert!(found[0].contains("out/bin/koji"), "{found:?}");
            assert!(found[0].contains(".shepignore"), "{found:?}");
        }
    }

    #[test]
    fn an_ordinary_shared_file_is_not_named() {
        let artifacts = [PathBuf::from("out/bin/koji")];
        let found = warnings(
            "web",
            &linked(&[".env", "config/local.json", "Flockfile.override.toml"]),
            &artifacts,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_name_that_only_contains_a_build_directory_name_is_not_named() {
        let found = warnings("web", &linked(&["distinct", "my_dist", "dist.log"]), &[]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_artifact_that_names_no_component_overlaps_nothing() {
        let artifacts = [PathBuf::from(".")];
        let found = warnings("web", &linked(&[".env"]), &artifacts);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_warning_cannot_forge_a_line() {
        let path = "dist/x\nshep-deploy: web: forged";
        let found = warnings("web", &linked(&[path]), &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].contains('\n'), "{found:?}");
    }
}
