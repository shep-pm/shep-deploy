//! The operator's `.shepignore`: what it names, and whether a path is named by it.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::Error;

/// One `.shepignore` entry, with the two things it can mean told apart at
/// parse time rather than at every match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// A bare name: matches a path component at any depth, so `node_modules`
    /// excludes the top-level directory and every `packages/*/node_modules`
    /// beneath it. `.gitignore`'s own rule for a pattern with no `/`.
    Anywhere(String),
    /// A path: matches only that subtree from the checkout root, so
    /// `packages/app/dist` never touches a top-level `dist`. Written with a
    /// `/` inside it, or with a leading `/`, which `.gitignore` also reads
    /// as "anchor this here".
    Anchored(PathBuf),
}

/// The patterns listed in `checkout`'s `.shepignore`, one per line, blank
/// lines and `#`-comments dropped.
///
/// An absent file is not an error: it returns an empty list, which is the
/// zero-configuration case - no `.shepignore` means share everything
/// [`ignored_present`](super::link::ignored_present) finds.
///
/// Every spelling `.gitignore` allows and this file does not is refused with
/// [`Error::Config`] naming the pattern, never accepted and silently matched
/// against nothing: a glob (`*`, `?`, `[`), a negation (`!name`), any
/// backslash escape other than `\!`, and a `..` component. `.shepignore`'s syntax is narrower than `.gitignore`'s, see
/// [`to_link`](super::link::to_link) for what it does support, and an operator who writes `*.log`
/// believing otherwise deserves a failure they see immediately rather than
/// an artifact this subtraction was built to keep out quietly staying shared
/// forever because the glob never matched anything.
///
/// Three spellings `.gitignore` allows ARE honoured, because each used to be
/// accepted and then matched nothing: a leading `/` anchors the pattern to
/// the checkout root, a leading `./` is dropped, and a trailing `/` is not
/// part of the name. Before 2026-09-03 `/dist` was read as a two-component
/// path that no relative entry could ever start with, so the build output it
/// named stayed shared. `\!name` names a file that really begins with `!`.
///
/// # Errors
/// [`Error::Io`], naming `checkout/.shepignore`, if the file exists but
/// cannot be read for any reason other than simply not being there.
/// [`Error::Config`] if any pattern is one of the refused spellings above.
pub fn shepignore_patterns(checkout: &Path) -> Result<Vec<Pattern>, Error> {
    let path = checkout.join(".shepignore");

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path, source }),
    };

    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(parse_pattern)
        .collect()
}

/// One trimmed, non-blank, non-comment `.shepignore` line as a [`Pattern`].
///
/// # Errors
/// [`Error::Config`] naming the line, for a glob, a negation, a backslash
/// other than the leading `\!` escape, a `..` component, or a line that is
/// nothing but slashes and dots.
fn parse_pattern(line: &str) -> Result<Pattern, Error> {
    let refuse = |why: &str| {
        Error::Config(format!(
            ".shepignore pattern {line:?} {why} - a .shepignore pattern is a bare name \
             (matches at any depth) or a path containing `/` (anchored to the checkout root), \
             nothing else"
        ))
    };

    if line.contains(['*', '?', '[']) {
        return Err(refuse(
            "uses glob syntax (`*`, `?`, `[`), which is not supported",
        ));
    }
    if line.starts_with('!') {
        return Err(refuse(
            "is a negation, which is not supported: everything is shared unless named here. A \
             name that really begins with `!` is written `\\!name`, as in .gitignore",
        ));
    }
    // `.gitignore`'s escape for a name that begins with `!`, honoured for the
    // same reason the bare form is refused: the two spellings mean different
    // things, and this one is the only way to name such a file.
    let unescaped;
    let line = match line.strip_prefix("\\!") {
        Some(rest) => {
            unescaped = format!("!{rest}");
            unescaped.as_str()
        }
        None => line,
    };
    // Every other backslash is a `.gitignore` escape this file does not
    // read, and a pattern carrying one would be kept verbatim and match
    // nothing: `\#name` never names `#name`.
    if line.contains('\\') {
        return Err(refuse(
            "carries a backslash; the only escape this file reads is a leading `\\!`",
        ));
    }

    let anchored_by_slash = line.starts_with('/');
    // Leading `/` and `./` are stripped until neither is left: one pass
    // each left `.//x` as `/x`, which no relative entry starts with. What
    // remains is either empty, or a body whose first component is a name.
    let mut body = line;
    loop {
        let stripped = body.trim_start_matches('/').trim_start_matches("./");
        if stripped == body {
            break;
        }
        body = stripped;
    }
    let body = body.trim_end_matches('/');
    // Nothing left, or a lone `.`, is a name no path component ever has.
    // `Path` keeps only a leading `.` as a component and drops interior ones,
    // so this catches `.` and `/./` while `x/./y` is left alone: it matches
    // as `x/y`, the same way on both sides of the comparison.
    if body.is_empty()
        || Path::new(body)
            .components()
            .any(|component| component == Component::CurDir)
    {
        return Err(refuse("names nothing"));
    }
    if Path::new(body)
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(refuse(
            "climbs out of the checkout with `..`, so it cannot name anything in it",
        ));
    }

    if anchored_by_slash || body.contains('/') {
        Ok(Pattern::Anchored(PathBuf::from(body)))
    } else {
        Ok(Pattern::Anywhere(body.to_owned()))
    }
}

/// Whether `path` (relative to the checkout) is named by `pattern`.
///
/// The two arms are the two meanings [`Pattern`] documents. Wildcards never
/// reach here: [`shepignore_patterns`] refuses them before [`to_link`](super::link::to_link) ever
/// calls this.
pub(super) fn pattern_matches(path: &Path, pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Anywhere(name) => path
            .components()
            .any(|component| component.as_os_str() == name.as_str()),
        Pattern::Anchored(prefix) => path.starts_with(prefix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::fixtures;

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

    /// fails if `.shepignore` parsing stops skipping blank lines, or stops
    /// skipping `#` comments - two separate clauses of the same filter, and
    /// a filter proven on one clause can still be broken on the other.
    /// `shepignored_paths_are_not_linked` above only exercises a
    /// `.shepignore` with neither blank lines nor comments in it, so this
    /// is the only test standing between either clause and going unguarded.
    #[test]
    fn shepignore_patterns_skips_blank_lines_and_comments() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (
                ".shepignore",
                "# build output, never share this\n\ndist\n\n",
            ),
            ("dist/app.js", "//"),
        ]);
        let patterns = shepignore_patterns(repo.path()).expect("reads");
        assert_eq!(patterns, vec![Pattern::Anywhere("dist".to_owned())]);
    }

    /// fails if `\!name` stops naming a file that really begins with `!`,
    /// or if a lone `.` is accepted. The first is `.gitignore`'s own escape
    /// and the only way to name such a file once the bare form is refused;
    /// the second is a name no path component ever has.
    #[test]
    fn an_escaped_bang_names_the_file_and_a_lone_dot_is_refused() {
        assert_eq!(
            parse_pattern("\\!weird").expect("the escape is honoured"),
            Pattern::Anywhere("!weird".to_owned())
        );
        for pattern in [".", "./.", "./", ".//", "/./"] {
            let err = parse_pattern(pattern).expect_err(pattern);
            assert!(
                err.to_string().contains("names nothing"),
                "{pattern}: {err}"
            );
        }
        // And the spellings that mean something still parse to it. `/./x`
        // used to come out as `Anchored("./x")`, which no entry starts with.
        assert_eq!(
            parse_pattern("./dist").expect("a leading ./ is dropped"),
            Pattern::Anywhere("dist".to_owned())
        );
        assert_eq!(
            parse_pattern("/./x").expect("a leading /./ is an anchor"),
            Pattern::Anchored(PathBuf::from("x"))
        );
        assert_eq!(
            parse_pattern(".//x").expect("stripped to a fixed point"),
            Pattern::Anywhere("x".to_owned())
        );
        let err = parse_pattern("\\#name").expect_err("an escape this file does not read");
        assert!(err.to_string().contains("backslash"), "{err}");
    }

    /// fails if a spelling this file cannot honour is accepted and silently
    /// matches nothing. Each of these is valid `.gitignore`, and each used
    /// to parse: `!dist` as a name beginning with `!`, `../x` as a path no
    /// entry starts with, `/` as an empty name.
    #[test]
    fn a_negation_a_parent_component_and_a_bare_slash_are_refused_by_name() {
        for pattern in ["!dist", "../sibling", "/", "./"] {
            let repo = fixture_repo(&[(".gitignore", "dist/\n"), (".shepignore", pattern)]);
            let err = shepignore_patterns(repo.path())
                .expect_err(&format!("{pattern:?} must be refused"));
            assert!(matches!(err, Error::Config(_)), "{pattern:?}: {err}");
            assert!(
                err.to_string().contains(pattern),
                "the refusal must name the pattern: {err}"
            );
        }
    }

    /// operator writing `*.log`, trusting the spec's "same idiom as
    /// .gitignore" line, must get a loud failure naming the pattern rather
    /// than a glob that silently matches nothing forever while the
    /// artifact it named stays shared.
    #[test]
    fn shepignore_refuses_a_pattern_with_an_asterisk() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "*.log\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("*.log"));
    }

    /// fails if a `.shepignore` pattern using `?` stops being refused - the
    /// second of the three metacharacters `pattern_matches` never gets a
    /// chance to mishandle, since none of them are meant to reach it.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_question_mark() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache?.tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache?.tmp"));
    }

    /// fails if a `.shepignore` pattern using a `[...]` class stops being
    /// refused - the third metacharacter, and the one most likely to be
    /// dropped from a hand-written `contains` check without a test naming
    /// it specifically.
    #[test]
    fn shepignore_refuses_a_pattern_with_a_bracket_class() {
        let repo = fixture_repo(&[
            (".gitignore", "dist/\n"),
            (".shepignore", "cache[0-9].tmp\n"),
            ("dist/app.js", "//"),
        ]);
        let err = shepignore_patterns(repo.path()).expect_err("must refuse a glob pattern");
        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("cache[0-9].tmp"));
    }
}
