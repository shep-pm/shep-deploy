//! Deriving which of the operator's checkout files a fresh release needs.
//!
//! A fresh `git worktree` contains nothing git ignores, so it cannot run:
//! ReactMap needs `config/local.json`, a generated masterfile and several
//! more, none of which are in the repository. Something has to put them
//! there, and the rule is the operator's own checkout plays the role a
//! `shared/` directory would otherwise play - see [`to_link`].
//!
//! Three steps, one function each: [`ignored_present`] asks git what it
//! ignores and finds present on disk right now, [`shepignore_patterns`]
//! reads the operator's opt-out list, and [`to_link`] subtracts the second
//! from the first. [`link_into`] is the only function here that writes
//! anything, and it never writes to the checkout - only into a release.
//!
//! `FromCheckout` and `to_link` share this file because the type's field is
//! private, and field privacy is per module.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::process::run_git;
use super::shepignore::{pattern_matches, shepignore_patterns};
use crate::error::Error;

mod within;

#[cfg(test)]
mod into_tests;

/// Every path in `checkout` that git ignores and that exists on disk right
/// now, relative to `checkout`.
///
/// Asks `git status --ignored=matching --porcelain` rather than parsing
/// `.gitignore` by hand. Parsing gets negations
/// (`!server/src/configs/.gitkeep`), anchored globs (`/docker-compose.yml`)
/// and nested ignore files in subdirectories wrong; git already answers
/// this question correctly, because it is git's own question to answer.
///
/// `=matching` (over the porcelain default, which git calls `traditional`)
/// is what keeps a wholly-ignored directory as one entry - `node_modules/`,
/// correctly, since it is meant to move as a single symlink - while still
/// naming an individually-ignored file on its own when it sits beside
/// ordinary tracked content, such as `config/local.json` next to a tracked
/// `config/schema.sql`. The default `traditional` mode collapses both cases
/// down to the containing directory, which would make a single ignored
/// file impossible to symlink without dragging its tracked siblings along
/// as dangling links.
///
/// `-z` terminates each entry with NUL instead of newline, and it is the
/// only output mode in which a path is never quoted. Without it git wraps
/// any path holding a space, a quote or a non-ASCII byte in `"..."` with
/// C-style escapes, `core.quotePath=false` notwithstanding, which covers only
/// the non-ASCII case. The quotes then became part of the path: a
/// gitignored `sp ace` was linked into the release as `"sp ace"`, pointing at
/// a file of that name that does not exist, and `symlink(2)` does not check.
/// Measured 2026-09-03.
///
/// # Errors
/// [`Error::Io`] if `git` cannot be launched or answers with non-UTF-8
/// bytes; [`Error::Git`] if it launches but exits non-zero.
pub fn ignored_present(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let stdout = run_git(
        checkout,
        &["status", "--ignored=matching", "--porcelain", "-z"],
    )?;

    // Walked as records rather than filtered as fields: a rename or a copy
    // (`R` or `C` in either status column) is followed by a second field
    // holding the original path, with no status of its own, and a bare field
    // must not be read as an entry.
    let mut fields = stdout.split('\0');
    let mut found = Vec::new();
    while let Some(entry) = fields.next() {
        let Some((status, path)) = entry.split_at_checked(3) else {
            continue;
        };
        if status.as_bytes()[..2]
            .iter()
            .any(|b| matches!(b, b'R' | b'C'))
        {
            fields.next();
        }
        if status == "!! " {
            found.push(PathBuf::from(path.trim_end_matches('/')));
        }
    }
    Ok(found)
}

/// The relative paths a release shares from the operator's own checkout.
///
/// Only [`to_link`] builds one, and that is the whole point of the type.
/// [`crate::flockfile`] treats the presence of `Flockfile.override.toml` in
/// this list as proof that the override is the operator's file rather than
/// one the repository committed, and proof has to come from the thing that
/// did the linking. A plain slice any caller could assemble was the same
/// evidence with no chain of custody: a wrong or lazy caller could mint the
/// answer. With the constructor private the only way to hold one is to have
/// asked [`to_link`], which read the operator's checkout to make it.
///
/// Read-only afterwards. It derefs to a slice for [`link_into`] and the
/// tests, and answers [`Self::includes_override`] for `flockfile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromCheckout(Vec<PathBuf>);

impl FromCheckout {
    /// Whether the operator's `Flockfile.override.toml` is among the paths.
    ///
    /// Answered from this list, not from the filesystem. Three versions of
    /// this check asked the filesystem and all three were wrong, the last
    /// one subtly enough to be worth recording.
    ///
    /// It required the override to resolve OUTSIDE the release, on the
    /// grounds that the operator's arrives as a symlink into their checkout.
    /// A repository can satisfy that in two commits. Release A ships an
    /// ordinary tracked file holding `user = "root"` under some innocuous
    /// name, and goes live, which by this crate's own invariant means
    /// `current` points at it. Release B then commits
    /// `Flockfile.override.toml` as a symlink to
    /// `../../current/.deploy-payload.toml`. That resolves into release A,
    /// which is outside release B, so the check passed and the refusal was
    /// skipped. Demonstrated 2026-08-28.
    ///
    /// The lesson is not that the rule needed another clause. It is that
    /// provenance cannot be recovered from a path once the path exists:
    /// whoever can write the tree can write the evidence. [`to_link`] is
    /// the only thing that knows which files came from the operator's
    /// checkout, because it is what chose them, so the answer travels from
    /// there instead of being reconstructed later.
    #[must_use]
    pub fn includes_override(&self) -> bool {
        self.0
            .iter()
            .any(|path| path == Path::new(crate::flockfile::OVERRIDE))
    }

    /// A list the tests assemble by hand. Test builds only, so production
    /// code has exactly one way to hold one: asking [`to_link`].
    #[cfg(test)]
    pub fn of(paths: Vec<PathBuf>) -> Self {
        Self(paths)
    }
}

impl std::ops::Deref for FromCheckout {
    type Target = [PathBuf];

    fn deref(&self) -> &[PathBuf] {
        &self.0
    }
}

/// [`ignored_present`], minus whatever `.shepignore` names, except the
/// operator's own `Flockfile.override.toml`, which nothing can filter out.
///
/// This subtraction is the entire reason `.shepignore` exists. `.gitignore`
/// conflates config that must be shared, caches that may be, and build
/// outputs that must not, because git ignores all three for the same
/// reason. Symlinking a build output back to a single shared copy means the
/// next release's build writes straight through that link, replacing the
/// assets the current release is serving mid-build; rolling back afterwards
/// then serves the new build's output under the old release's name. That
/// kills blue/green and rollback together, which is why this subtraction
/// must never be skipped.
///
/// # Errors
/// Whatever [`ignored_present`] or [`shepignore_patterns`] returns.
pub fn to_link(checkout: &Path) -> Result<FromCheckout, Error> {
    let ignored = ignored_present(checkout)?;
    let patterns = shepignore_patterns(checkout)?;

    Ok(FromCheckout(
        ignored
            .into_iter()
            .filter(|path| {
                // The operator's override is the one entry `.shepignore` cannot
                // reach. `.shepignore` is committed by the deployed repository,
                // and this list is the whole of the evidence
                // `FromCheckout::includes_override` has that the override came
                // from the operator rather than from the repo. A repo that can delete the
                // entry deletes every pin the operator wrote in the override,
                // `user` among them, and a build pinned to an unprivileged
                // account runs as the dog's own uid instead. Silently: nothing
                // errors, because an absent override is a legitimate state.
                path == Path::new(crate::flockfile::OVERRIDE)
                    || !patterns
                        .iter()
                        .any(|pattern| pattern_matches(path, pattern))
            })
            .collect(),
    ))
}

/// Symlinks every path in `paths` from `checkout` into `release`, creating
/// whatever parent directories the release needs along the way.
///
/// `checkout` is canonicalised before anything is joined onto it. A
/// symlink's target text is stored exactly as given - `symlink()` performs
/// no resolution of its own - and the OS later resolves a relative target
/// against the *symlink's own containing directory*, not against this
/// process's working directory or against whatever the caller meant by
/// `checkout`. A relative `checkout` therefore produced a symlink whose
/// target text was embedded literally and dangled the moment anything
/// read through it: `symlink()` itself still succeeded, so the deploy
/// would carry on and the break would only surface after the swap and
/// after the reload, when something finally tried to read a shared file.
/// Canonicalising first makes the target text absolute regardless of what
/// form `checkout` arrived in, and turns a checkout that cannot be
/// resolved at all into an immediate, named error instead of a link that
/// looks fine until it is used.
///
/// Reads from `checkout` and writes only under `release` - the dog never
/// writes to the operator's own checkout, and any code path that would is a
/// bug. `paths` are relative, the same relative paths [`to_link`] returns,
/// and are joined onto both roots here: onto `checkout` to find the real
/// file, onto `release` to decide where its symlink belongs.
///
/// # Errors
/// [`Error::Io`], naming `checkout`, if it cannot be canonicalised - it
/// does not exist, or a component of it cannot be resolved. Otherwise
/// [`Error::Io`], naming the release-side path that failed, if a parent
/// directory cannot be opened or created or the symlink itself cannot be made.
/// A parent directory that is a symlink is refused, not followed: see
/// [`within::link_within`].
///
/// A release-side symlink that already points at the very target this would
/// make is accepted and left alone: a retry reuses a finished release, and
/// the links the earlier attempt made are still in it.
///
/// One collision is not an [`Error::Io`] at all. Any other release-side path
/// that already exists, a real file or a link aimed elsewhere, gives
/// [`Error::Config`] instead, because the cause is a
/// `.shepignore` that shares something the release builds for itself, which
/// the operator fixes by editing that file rather than by looking at the
/// filesystem. The message names the colliding path and the file to edit.
pub fn link_into(release: &Path, checkout: &Path, paths: &FromCheckout) -> Result<(), Error> {
    let checkout = fs::canonicalize(checkout).map_err(Error::at(checkout))?;

    for relative in paths.iter() {
        let target = checkout.join(relative);
        let link = release.join(relative);

        // A retry reuses a finished release, so the links the earlier attempt
        // made are still there. One aimed at exactly the target we would make
        // is that attempt's own, not a collision.
        let in_place = within::link_within(release, relative, &target).map_err(Error::at(&link))?;
        if !in_place {
            return Err(Error::Config(format!(
                "{} is already present in the release, so {} cannot be linked from the \
                 checkout. The usual cause is a build output that git ignores and \
                 `.shepignore` does not: this dog gives each sheep its own build cache and \
                 links it in first, and the operator's own build artifacts must not be \
                 shared into a release at all, because the next release's build would write \
                 through the link and replace what the current one is serving. Add {} to \
                 `.shepignore` in the checkout.",
                link.display(),
                relative.display(),
                relative.display()
            )));
        }
    }

    Ok(())
}

/// Points `release/target` at the dog's own build cache, creating the cache
/// if this is the first release to want it.
///
/// Runs BEFORE [`link_into`], so a checkout that shares its own `target`
/// (no `.shepignore`, which the design says is a misconfiguration rather
/// than a mode) collides in `link_into` where the error can name the fix,
/// rather than here where it cannot.
///
/// A release that already has a `target` path is left exactly as it is and
/// gets no cache. That is a repository which committed the directory, which
/// is unusual and its own business; overruling it would be this crate
/// deciding the repository's layout for it.
///
/// # Errors
/// [`Error::Io`], naming the cache, if it cannot be created; naming the
/// link, if the symlink cannot be made for any reason other than something
/// already being there.
pub fn link_cache(release: &Path, cache_target: &Path) -> Result<(), Error> {
    let link = release.join("target");
    // `symlink_metadata` answers for a directory, a file, a live link and a
    // dangling one alike, which is every "something is already there".
    if link.symlink_metadata().is_ok() {
        return Ok(());
    }

    fs::create_dir_all(cache_target).map_err(Error::at(cache_target))?;

    symlink(cache_target, &link).map_err(Error::at(link))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use tempfile::TempDir;

    /// [`fixtures::checkout`] under the name every test here calls it by.
    ///
    /// `git add` silently skips anything matched by `.gitignore`, so an entry
    /// meant to be "ignored and present" - `config/local.json` in the
    /// fixtures below - simply stays untracked while a `.gitignore` or
    /// `tracked.txt` entry lands in the commit. That is exactly the split
    /// every test here needs: something tracked, something ignored.
    fn fixture_repo(entries: &[(&str, &str)]) -> TempDir {
        fixtures::checkout(entries)
    }

    /// fails if enumeration stops using git's own answer. Parsing
    /// `.gitignore` by hand gets negations (`!server/src/configs/.gitkeep`),
    /// anchored globs (`/docker-compose.yml`) and nested ignore files
    /// wrong; `git status --ignored` gets all three right because it is
    /// git deciding.
    #[test]
    fn enumeration_asks_git_rather_than_parsing_gitignore() {
        let repo = fixture_repo(&[
            (".gitignore", "config/\n!config/.gitkeep\n"),
            ("config/local.json", "{}"),
            ("config/.gitkeep", ""),
            ("tracked.txt", "x"),
        ]);
        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("config")));
        assert!(!found.iter().any(|p| p.ends_with("tracked.txt")));
    }

    /// fails if a plain untracked-but-not-ignored file is treated as shared.
    /// `ignored_present` keeps only lines beginning `!! `; a file git status
    /// reports as `?? ` (untracked, not ignored) has no business in this
    /// list, and nothing in the test above proves that half of the filter -
    /// its only untracked-looking entry (`tracked.txt`) is committed, not
    /// merely present.
    #[test]
    fn ignored_present_excludes_untracked_files_that_are_not_ignored() {
        let repo = fixture_repo(&[(".gitignore", "dist/\n"), ("dist/app.js", "//")]);
        fs::write(repo.path().join("scratch.txt"), "untracked, not ignored")
            .expect("write scratch file");

        let found = ignored_present(repo.path()).expect("enumerates");
        assert!(found.iter().any(|p| p.ends_with("dist")));
        assert!(!found.iter().any(|p| p.ends_with("scratch.txt")));
    }

    /// fails if the provenance answer stops following the list. The
    /// override's presence among the linked paths is the only evidence
    /// `flockfile` has that the file is the operator's; an answer from
    /// anywhere else is one the repository can forge (see
    /// `FromCheckout::includes_override`).
    #[test]
    fn the_override_is_recognised_only_when_it_was_linked() {
        let repo = tempfile::tempdir().expect("tempdir");
        fixtures::run_git(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join(".gitignore"), "Flockfile.override.toml\n").expect("ignore");
        fs::write(
            repo.path().join("Flockfile.override.toml"),
            "[[app]]\nname = 'web'\nuser = 'app'\n",
        )
        .expect("override");

        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.includes_override(), "{linked:?}");

        fs::remove_file(repo.path().join("Flockfile.override.toml")).expect("removed");
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.includes_override(), "{linked:?}");
        assert!(!FromCheckout::of(vec![PathBuf::from("config/local.json")]).includes_override());
    }

    /// fails if a `.shepignore` entry is still linked. This is the whole
    /// reason `.shepignore` exists: symlinking a build output means release
    /// B's build writes through the link and replaces what release A is
    /// currently serving, which kills blue/green and rollback in one line.
    #[test]
    fn shepignored_paths_are_not_linked() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\nconfig/local.json\n"),
            (".shepignore", "dist\n"),
            ("dist/app.js", "//"),
            ("config/local.json", "{}"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.iter().any(|p| p.ends_with("config/local.json")));
        assert!(!linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a committed `.shepignore` can drop the operator's own
    /// override out of the shared list.
    ///
    /// `.shepignore` is a repo-committed file, so the deployed repository
    /// writes it. `Flockfile.override.toml` is the operator's, and
    /// `FromCheckout::includes_override` treats presence in this list as the whole
    /// proof of that. Letting the repo delete the entry silently drops every
    /// pin the operator put in the override, `user` included, which is the
    /// one that keeps a build off the dog's own uid. One innocuous-looking
    /// line, and a build the operator pinned to `svc` runs as root instead.
    #[test]
    fn a_committed_shepignore_cannot_drop_the_operators_override() {
        let repo = fixture_repo(&[
            (".gitignore", "Flockfile.override.toml\n"),
            (".shepignore", "Flockfile.override.toml\n"),
        ]);
        // The operator's own, present on disk and never committed, exactly
        // like the `config/local.json` the other fixtures here use.
        fs::write(
            repo.path().join("Flockfile.override.toml"),
            "[[app]]\nname = \"web\"\nuser = \"svc\"\n",
        )
        .expect("the operator's override");

        let linked = to_link(repo.path()).expect("computes");

        assert!(
            linked
                .iter()
                .any(|p| p.ends_with("Flockfile.override.toml")),
            "the override must survive a repo-committed .shepignore: {linked:?}"
        );
    }

    /// fails if a repo with no `.shepignore` stops sharing everything
    /// ignored. That is the zero-configuration case and the common one.
    #[test]
    fn no_shepignore_means_share_everything_ignored() {
        let repo = fixture_repo(&[(".gitignore", "config/\n"), ("config/local.json", "{}")]);
        assert!(!to_link(repo.path()).expect("computes").is_empty());
    }

    /// fails if a leading `/` stops anchoring a pattern to the checkout
    /// root. `.gitignore` reads `/dist` that way, so an operator will write
    /// it; before 2026-09-03 it parsed as a two-component path no relative
    /// entry could start with, and matched nothing while saying nothing.
    #[test]
    fn a_leading_slash_anchors_a_pattern_to_the_checkout_root() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "/dist\n"),
            ("dist/app.js", "//"),
            ("packages/app/dist/bundle.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.contains(&PathBuf::from("dist")), "{linked:?}");
        assert!(
            linked.contains(&PathBuf::from("packages/app/dist")),
            "anchored means only the root one: {linked:?}"
        );
    }

    /// fails if a trailing `/` is read as part of the name. `dist/` is how
    /// `.gitignore` spells "the directory", and the same operator copies the
    /// line across.
    #[test]
    fn a_trailing_slash_is_not_part_of_the_name() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "dist/\n"),
            ("dist/app.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.contains(&PathBuf::from("dist")), "{linked:?}");
    }

    /// fails if a rename in the operator's checkout is read as an ignored
    /// entry. With `-z` a rename is two fields, the second a bare path with
    /// no status, and a field-by-field filter would take a second field that
    /// happens to begin `!! ` for an entry.
    #[test]
    fn a_staged_rename_is_one_record_not_two() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            ("dist/app.js", "//"),
            ("old.txt", "x"),
        ]);
        // A file whose new name would parse as an ignored entry if its record
        // were split into two.
        fixtures::run_git(repo.path(), &["mv", "old.txt", "!! x"]);

        let found = ignored_present(repo.path()).expect("enumerates");

        assert!(found.contains(&PathBuf::from("dist")), "{found:?}");
        assert!(
            !found.iter().any(|p| p.ends_with("x")),
            "the rename's own path must not be read as ignored: {found:?}"
        );
    }

    /// fails if a bare `.shepignore` pattern stops matching a nested
    /// directory of the same name. ReactMap's real ignored set has
    /// `node_modules/` at the top level and several more nested under
    /// `packages/*/node_modules/`; a user writing `node_modules` in
    /// `.shepignore` means all of them, matching how a bare name in
    /// `.gitignore` itself matches at any depth.
    #[test]
    fn shepignore_bare_pattern_matches_at_any_depth() {
        let repo = fixture_repo(&[
            (".gitignore", "node_modules/\ndist/\n"),
            (".shepignore", "node_modules\n"),
            ("node_modules/a.js", "//"),
            ("packages/foo/node_modules/b.js", "//"),
            ("dist/app.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(!linked.iter().any(|p| p.ends_with("node_modules")));
        assert!(linked.iter().any(|p| p.ends_with("dist")));
    }

    /// fails if a `.shepignore` pattern containing `/` stops being anchored
    /// to the checkout root. A naive "match the last component anywhere"
    /// implementation would make `packages/dist` also exclude an unrelated
    /// top-level `dist`; the anchored rule must exclude only the exact
    /// subtree named.
    #[test]
    fn shepignore_pattern_with_slash_is_anchored_to_its_own_subtree() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\npackages/dist/\n"),
            (".shepignore", "packages/dist\n"),
            ("dist/app.js", "//"),
            ("packages/dist/bundle.js", "//"),
        ]);
        let linked = to_link(repo.path()).expect("computes");
        assert!(linked.contains(&PathBuf::from("dist")));
        assert!(!linked.contains(&PathBuf::from("packages/dist")));
    }

    /// fails if a release does not get a `target` pointing at the dog's own
    /// cache. Without it every deploy of a Rust project is a from-scratch
    /// build, which the design calls not acceptable for Koji specifically.
    #[test]
    fn a_release_gets_target_linked_at_the_cache() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let cache = root.path().join("cache/target");
        fs::create_dir_all(&release).expect("release dir");

        link_cache(&release, &cache).expect("links");

        let link = release.join("target");
        assert_eq!(fs::read_link(&link).expect("a symlink"), cache);
        assert!(cache.is_dir(), "the cache itself must be created");
    }

    /// fails if a release that ships its own tracked `target/` is treated
    /// as an error. A repository committing that directory is unusual and
    /// its own business; refusing the deploy over it would be this crate
    /// overruling the repository about the repository's own layout.
    #[test]
    fn a_release_that_ships_its_own_target_is_left_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        fs::create_dir_all(release.join("target")).expect("a committed target");
        let cache = root.path().join("cache/target");

        link_cache(&release, &cache).expect("does nothing, successfully");

        assert!(
            release.join("target").is_dir(),
            "the repository's own directory must survive"
        );
        assert!(
            fs::read_link(release.join("target")).is_err(),
            "and must not have been replaced by a link"
        );
    }

    /// fails if a checkout sharing its own `target` collides with the
    /// dog's cache and produces a bare "File exists". The two are genuinely
    /// different things, the operator's dev artifacts and the dog's shared
    /// cache, and the design says the operator's own target directory must
    /// never be linked. The fix is one line in `.shepignore`, so the error
    /// says so.
    #[test]
    fn a_checkout_sharing_target_says_how_to_fix_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let release = root.path().join("release");
        let checkout = root.path().join("checkout");
        fs::create_dir_all(&release).expect("release");
        fs::create_dir_all(checkout.join("target")).expect("their own target");
        link_cache(&release, &root.path().join("cache/target")).expect("links");

        let err = link_into(
            &release,
            &checkout,
            &FromCheckout::of(vec![PathBuf::from("target")]),
        )
        .expect_err("collides");
        let shown = err.to_string();
        assert!(shown.contains(".shepignore"), "{shown}");
        assert!(shown.contains("target"), "{shown}");
    }
}
