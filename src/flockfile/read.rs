//! Reading one Flockfile from disk: bounded, without following a symlink the
//! repository committed, and parsed without quoting the file back.

use std::fs;
use std::io::{self, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use toml::{Table, Value};

use crate::error::Error;
use crate::shared::{is_eloop, o_nofollow, printable};

/// The most a Flockfile may be, in bytes.
///
/// A mebibyte, which is a thousand times any real one. Both files are read
/// whole and merged in memory, and both come from the deployed repository,
/// so without a bound a commit could hand this dog a file large enough to
/// hold the poll loop, and the tree's lock, for as long as it liked.
const MAX_FLOCKFILE_BYTES: u64 = 1024 * 1024;

/// The override's name, in the one place the check and the read use it.
///
/// `crate::shared::to_link` needs it too, to keep a repo-committed
/// `.shepignore` from filtering the operator's own file out of the list
/// `FromCheckout::includes_override` then answers from.
pub(crate) const OVERRIDE: &str = "Flockfile.override.toml";

/// Whether a Flockfile read may follow a symlink.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Follow {
    /// The file is the operator's, linked in from their checkout, so the
    /// link is the whole point.
    Operators,
    /// The file is the repository's, and a link is refused: see
    /// [`read_flockfile`].
    Never,
}

/// Reads and parses a Flockfile that must exist.
///
/// # Errors
/// [`Error::Io`] if `path` cannot be read at all. [`Error::Config`] if its
/// contents are not valid TOML, if it is a symlink and `follow` says not to
/// follow one, or if it is past [`MAX_FLOCKFILE_BYTES`].
pub(super) fn read_required(path: &Path, follow: Follow) -> Result<Value, Error> {
    let text = read_flockfile(path, follow)?;
    parse(path, &text)
}

/// As [`read_required`], but a missing file is not an error - it is treated
/// as an empty document, the same "absent means zero configuration"
/// precedent [`crate::shared::shepignore::shepignore_patterns`] already sets for
/// `.shepignore`. `Flockfile.override.toml` is optional by design.
///
/// # Errors
/// As [`read_required`], except that an absent file is an empty document.
pub(super) fn read_optional(path: &Path, follow: Follow) -> Result<Value, Error> {
    match read_flockfile(path, follow) {
        Ok(text) => parse(path, &text),
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(Value::Table(Table::new()))
        }
        Err(err) => Err(err),
    }
}

/// Reads one Flockfile whole, bounded, and without following a symlink
/// unless the file is the operator's.
///
/// The symlink refusal is a security boundary rather than tidiness. This
/// read runs in the dog's own process at its own uid, which under the
/// arrangement shep's docs recommend is root, and it runs BEFORE any build
/// drops to `user`. `git worktree add` materialises a committed symlink
/// exactly as committed, so `Flockfile.toml -> /root/.ssh/id_rsa` in the
/// tracked branch had this read the key, fail to parse it, and print its
/// first line into the log through the parser's own error. `user` is the
/// one bound this crate names as covering what a build can READ, and a
/// commit walked around it without running a build at all. Found in review
/// on 2026-09-03.
///
/// `O_NOFOLLOW` makes the kernel refuse the open when the last component is
/// a link. A component above it is not this function's concern: those are
/// the release directory and the tree, which only this dog writes.
///
/// # Errors
/// [`Error::Config`] naming `path` if it is a symlink this read may not
/// follow, or if it is larger than [`MAX_FLOCKFILE_BYTES`]. [`Error::Io`]
/// naming `path` for anything else, `NotFound` included.
fn read_flockfile(path: &Path, follow: Follow) -> Result<String, Error> {
    let at = |source| Error::Io {
        path: path.to_owned(),
        source,
    };

    let mut options = fs::OpenOptions::new();
    options.read(true);
    if follow == Follow::Never {
        options.custom_flags(o_nofollow());
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(source) if is_eloop(&source) => {
            return Err(Error::Config(format!(
                "{} is a symlink, and a Flockfile the repository commits has to be a regular \
                 file: this dog reads it at its own uid before any build drops privilege, so a \
                 link could point it at any file on the host. Commit the file itself.",
                path.display()
            )));
        }
        Err(source) => return Err(at(source)),
    };

    let len = file.metadata().map_err(at)?.len();
    if len > MAX_FLOCKFILE_BYTES {
        return Err(Error::Config(format!(
            "{} is {len} bytes, past the {MAX_FLOCKFILE_BYTES}-byte limit for a Flockfile. A \
             real one is a few hundred bytes; this dog reads and merges both files in memory \
             on every deploy.",
            path.display()
        )));
    }

    let mut text = String::with_capacity(usize::try_from(len).unwrap_or(0));
    file.read_to_string(&mut text).map_err(at)?;
    Ok(text)
}

/// Parses `text` as a generic TOML document, naming `path` and the line in
/// the error if it does not parse.
///
/// The parser's own `Display` quotes the offending line back, which is
/// right for a file the reader wrote and wrong for one the repository did:
/// the read above refuses a symlink so that no file on the host can reach
/// this parser, and this keeps the text out of the log in case one ever
/// does. The message and the line number are what an operator needs; the
/// line's text is in the file.
fn parse(path: &Path, text: &str) -> Result<Value, Error> {
    toml::from_str(text).map_err(|source| {
        // Counted over bytes, because a span's start is a byte offset and
        // slicing a `str` at a byte that is not a character boundary panics,
        // on text the repository wrote.
        let line = source.span().map(|span| {
            let upto = span.start.min(text.len());
            text.as_bytes()[..upto]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1
        });
        // `printable`, because serde's own message carries the offending
        // key by name, and a TOML key can be any quoted string.
        let message = printable(source.message());
        Error::Config(match line {
            Some(line) => format!("{}: {message}, at line {line}", path.display()),
            None => format!("{}: {message}", path.display()),
        })
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::flockfile::app_config;

    /// fails if a committed Flockfile that is a symlink is read at all.
    ///
    /// `git worktree add` materialises a committed symlink exactly as
    /// committed, and this read runs at the dog's own uid before any build
    /// drops privilege. `Flockfile.toml -> <any file root can read>` then had
    /// the parser fail on that file and quote its first line into the log.
    /// The assertion is on the secret NOT being in the message: an error
    /// alone is what the vulnerable version returned too, after reading it.
    #[test]
    fn a_committed_flockfile_that_is_a_symlink_is_refused_unread() {
        let rel = tempfile::tempdir().expect("tempdir");
        let secret = rel.path().join("outside.txt");
        std::fs::write(&secret, "-----BEGIN SECRET-----\nnot toml\n").expect("secret");
        std::os::unix::fs::symlink(&secret, rel.path().join("Flockfile.toml")).expect("link");

        let err = app_config(rel.path(), "web", &[]).expect_err("a link must be refused");

        let shown = err.to_string();
        assert!(matches!(err, Error::Config(_)), "{shown}");
        assert!(shown.contains("symlink"), "must say why: {shown}");
        assert!(
            !shown.contains("SECRET"),
            "must not quote the file: {shown}"
        );
    }

    /// fails if the operator's own override stops resolving through the
    /// symlink `shared::link_into` made for it. Refusing every link would
    /// have broken the one legitimate case, which is the whole mechanism.
    #[test]
    fn the_operators_override_is_still_followed() {
        let rel = tempfile::tempdir().expect("tempdir");
        let checkout = tempfile::tempdir().expect("checkout");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::fs::write(
            checkout.path().join(OVERRIDE),
            "[[app]]\nname = \"web\"\nuser = \"svc\"\n",
        )
        .expect("the operator's override");
        std::os::unix::fs::symlink(checkout.path().join(OVERRIDE), rel.path().join(OVERRIDE))
            .expect("linked in, as link_into does");

        let app = app_config(rel.path(), "web", &[PathBuf::from(OVERRIDE)])
            .expect("the operator's link resolves");
        assert_eq!(app.user.as_deref(), Some("svc"));
    }

    /// fails if a parse error quotes the offending line. The read refuses a
    /// symlink so no file on the host can reach the parser, and this is the
    /// second wall: even a file that does reach it is described by line
    /// number, not by content.
    #[test]
    fn a_parse_error_names_the_line_and_not_its_text() {
        let rel = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            rel.path().join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\n-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("not toml");

        let shown = err.to_string();
        assert!(shown.contains("line 3"), "must name the line: {shown}");
        assert!(!shown.contains("OPENSSH"), "must not quote it: {shown}");
    }

    /// fails if a Flockfile of any size is read whole. Both files come from
    /// the repository and are merged in memory on every deploy, with the
    /// tree's lock held.
    #[test]
    fn a_flockfile_past_the_size_limit_is_refused() {
        let rel = tempfile::tempdir().expect("tempdir");
        let mut big = String::from("[[app]]\nname = \"web\"\nscript = \"./run\"\n");
        big.push_str(&"# padding\n".repeat(120_000));
        assert!(
            big.len() as u64 > MAX_FLOCKFILE_BYTES,
            "the fixture must exceed the limit"
        );
        std::fs::write(rel.path().join("Flockfile.toml"), big).expect("committed");

        let err = app_config(rel.path(), "web", &[]).expect_err("too big");
        assert!(err.to_string().contains("limit"), "{err}");
    }

    /// fails if a repository can buy the exemption with a symlink out.
    ///
    /// The escape the previous version of the provenance check allowed, in two
    /// commits and needing nothing from the operator. Release A ships an
    /// ordinary tracked file holding `user = "root"` and goes live, so
    /// `current` points at it. Release B commits `Flockfile.override.toml` as
    /// a symlink to `../../current/.deploy-payload.toml`, which resolves into
    /// release A. That is outside release B, so the old "resolves outside the
    /// release" test passed and the refusal was skipped.
    ///
    /// It is the reason provenance now comes from the share list rather than
    /// from the filesystem: whoever can write the tree can write the evidence.
    /// Since 2026-09-03 the link is refused before its target is read at all,
    /// because a committed override is read without following symlinks; the
    /// privilege refusal behind it is pinned by
    /// `a_committed_override_cannot_grant_a_user`.
    #[test]
    fn a_committed_symlink_out_of_the_release_buys_no_exemption() {
        let tree = tempfile::tempdir().expect("tempdir");
        let a = tree.path().join("releases/shaA");
        let b = tree.path().join("releases/shaB");
        std::fs::create_dir_all(&a).expect("a");
        std::fs::create_dir_all(&b).expect("b");

        std::fs::write(
            a.join(".deploy-payload.toml"),
            "[[app]]\nname = \"web\"\nuser = \"root\"\n",
        )
        .expect("payload");
        std::os::unix::fs::symlink(&a, tree.path().join("current")).expect("current");

        std::fs::write(
            b.join("Flockfile.toml"),
            "[[app]]\nname = \"web\"\nscript = \"./run\"\n",
        )
        .expect("committed");
        std::os::unix::fs::symlink(
            "../../current/.deploy-payload.toml",
            b.join("Flockfile.override.toml"),
        )
        .expect("committed symlink");

        // Nothing was shared, so nothing is the operator's.
        let err =
            app_config(&b, "web", &[]).expect_err("a committed symlink must not buy the exemption");
        let shown = format!("{err}");
        assert!(
            shown.contains("symlink"),
            "the link is refused before it is followed: {shown}"
        );
        assert!(
            !shown.contains("root"),
            "and nothing of the payload is read: {shown}"
        );
    }
}
