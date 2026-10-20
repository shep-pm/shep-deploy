//! Helpers shared across the dog, grouped by concern.
//!
//! - [`link`]: which of the operator's checkout files a release shares, and
//!   linking them in.
//! - [`process`]: running `git` under a budget, killing a process group, and
//!   moving blocking work off the runtime.
//! - [`shepignore`]: the operator's `.shepignore` opt-out list.
//!
//! What is left here is small and has no better home: making a string safe to
//! log, `O_NOFOLLOW` and `ELOOP`, and comparing two spellings of one path.

mod link;
mod process;
pub(crate) mod shepignore;

pub use link::{FromCheckout, link_cache, link_into, to_link};
pub(crate) use process::{kill_group, off_thread, run_git, run_git_within};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `text` with every control character replaced by `?`, for a message that
/// quotes something the deployed repository chose.
///
/// `Display` and `Path::display` pass control bytes through, so a committed
/// `artifacts = ["\u{1b}[2J..."]` would write raw terminal escapes into the
/// dog's log, and a newline would forge a second log line. Replaced rather
/// than escaped, because the reader wants to recognise the entry, not
/// reconstruct it: the file is right there.
pub(crate) fn printable(text: impl std::fmt::Display) -> String {
    text.to_string()
        .chars()
        .map(|c| if forges_a_line(c) { '?' } else { c })
        .collect()
}

/// Whether `c` can change how a log line is read: a control character, or
/// one of the Unicode format and separator characters that reverse, split
/// or hide text (the bidi marks, overrides and isolates, the zero-width and
/// soft-hyphen family, the line and paragraph separators, the byte order
/// mark, and the tag block that carries invisible text). None of them
/// belongs in a file name or a TOML key an operator meant. Shared with
/// `paths::is_sheep_name` and `State::validate`, so what is refused in a
/// name is the same set that is replaced in a message.
pub(crate) fn forges_a_line(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// `O_NOFOLLOW`, without a libc dependency or an unsafe block.
///
/// The value is fixed by each platform's ABI and cannot change without
/// breaking every compiled program on it. Same "ask the host rather than
/// reimplement its answer" spirit as `crate::build`'s use of `id`, except
/// here the host's answer is a constant. Shared by the artifact copy and the
/// Flockfile read, which both open a path the deployed repository chose.
pub(crate) const fn o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400_000
    }
    #[cfg(target_vendor = "apple")]
    {
        0x0100
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        compile_error!("O_NOFOLLOW's value is not known for this target")
    }
}

/// Whether `err` is `ELOOP`: the kernel refusing to follow a symlink, which
/// is what an `O_NOFOLLOW` open answers when the last component is one.
///
/// By errno rather than `io::ErrorKind::FilesystemLoop`, which is not stable
/// yet. `nix` already names the number for each platform.
pub(crate) fn is_eloop(err: &io::Error) -> bool {
    err.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32)
}

/// `path` with every symlink and `..` resolved as far as the filesystem
/// allows, for comparing two spellings of one place.
///
/// The dog and the daemon can resolve `$SHEP_HOME` to different spellings
/// of the same directory (one through a symlinked home, say), and the
/// daemon hands back the spelling it registered. A literal comparison then
/// calls two names for one directory different, and both places that
/// compare a registered `cwd` with a tree's `current` decide something
/// irreversible on the answer: whether a sheep is already running from the
/// tree (`optin::prepare`, which otherwise tells the operator to remove
/// it), and whether the cutover registered it (`restore`, which otherwise
/// leaves it there).
///
/// Three rungs, each taken only when the one above cannot be. The whole
/// path canonicalised; failing that, the parent canonicalised with the
/// last component kept as written, which is what a `current` that dangles
/// needs, since the link itself is real even when its target is gone; and
/// failing that too, the path as given. Never an error: a path that cannot
/// be resolved at all is compared as spelled, the way it always was.
#[must_use]
pub fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(real_parent) = fs::canonicalize(parent)
    {
        return real_parent.join(name);
    }
    path.to_path_buf()
}

/// Whether `a` and `b` name the same place, by [`resolved`].
#[must_use]
pub fn same_path(a: &Path, b: &Path) -> bool {
    resolved(a) == resolved(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if two spellings of one place compare unequal, or if a
    /// `current` whose release is gone stops comparing at all. The dog and
    /// the daemon can spell `$SHEP_HOME` differently, and both comparisons
    /// that use this decide something irreversible on the answer.
    #[test]
    fn two_spellings_of_one_path_resolve_alike_even_when_the_target_is_gone() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        fs::create_dir_all(home.join("deploy/web")).expect("tree");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&home, &link).expect("a second spelling");

        let current = home.join("deploy/web/current");
        let through_link = link.join("deploy/web/current");
        // Dangling: the link exists, what it names does not.
        std::os::unix::fs::symlink(home.join("deploy/web/releases/gone"), &current)
            .expect("current");
        assert!(
            same_path(&current, &through_link),
            "parent resolves, name kept"
        );

        fs::remove_file(&current).expect("unlink");
        assert!(
            same_path(&current, &through_link),
            "nothing exists but the parent"
        );
        assert!(!same_path(&current, &home.join("deploy/web/previous")));
        assert_eq!(
            resolved(Path::new("/nonexistent/anywhere/x")),
            PathBuf::from("/nonexistent/anywhere/x"),
            "unresolvable stays as spelled"
        );
    }

    /// fails if `printable` lets a character through that can reverse,
    /// split or hide a log line. Control characters are the obvious half;
    /// the bidi override and the zero-width space are format characters
    /// `is_control` does not cover.
    #[test]
    fn printable_replaces_everything_that_can_forge_a_line() {
        let shown = printable("a\u{1b}[2Jb\u{202e}c\u{200b}d\ne\u{061c}f\u{e0041}g");
        assert_eq!(shown, "a?[2Jb?c?d?e?f?g");
        assert_eq!(printable("plain/name.txt"), "plain/name.txt");
    }
}
