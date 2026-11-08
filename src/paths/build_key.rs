//! What names one built release, and tells one build's output from
//! another's. See [`BuildKey`] for what a release is keyed by.

use std::collections::BTreeMap;

use crate::state::is_sha;

use super::fnv1a;

/// How [`BuildKey`] is spelled in a release directory name: a `u64` in hex.
const BUILD_KEY_WIDTH: usize = 16;

/// What tells one sheep's build of a sha from another's: the hash of the
/// `command`, the build `user` and the `build_env` that release was built by.
///
/// A release is a checkout plus whatever a build did to it. Two sheep of one
/// repository that build the same sha the same way want the same release, and
/// building it once is the point; two sheep whose build differs in any of
/// those three cannot share one, because each would find the other's output
/// where its own should be. So all three are in the key, and a release
/// directory is named `<sha>-<key>`.
///
/// `user` is in it for a second reason: a root dog hands a release to its
/// build user (see `crate::ownership::hand_over`), and two users must never
/// own one directory.
///
/// The three are hashed length-prefixed, so no pair of values reads as another
/// pair: `command = "a"` with `build_env = {"b" = "c"}` and `command = "ac"`
/// with `build_env = {}` cannot land on one key. `build_env` is hashed by
/// value, not by name, so a rotated secret is a different release - which is
/// right, since the build that consumed the old value is not reusable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildKey(String);

impl BuildKey {
    /// The key for the build that runs `command` as `user` with `env`.
    ///
    /// `env` is the resolved one: what the build actually gets, not what the
    /// settings file spells.
    #[must_use]
    pub fn of(command: Option<&str>, user: Option<&str>, env: &BTreeMap<String, String>) -> Self {
        let mut keyed = String::new();
        field(&mut keyed, command.unwrap_or(""));
        field(&mut keyed, user.unwrap_or(""));
        keyed.push_str(&env.len().to_string());
        keyed.push(':');
        for (name, value) in env {
            field(&mut keyed, name);
            field(&mut keyed, value);
        }
        Self(format!("{:016x}", fnv1a(&keyed)))
    }

    /// The key as it appears in a release directory name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `text` written into `keyed` so a reader of the whole string can tell where
/// it ends: its byte length, a `:`, then the bytes.
fn field(keyed: &mut String, text: &str) {
    keyed.push_str(&text.len().to_string());
    keyed.push(':');
    keyed.push_str(text);
}

/// The sha a release directory name carries: everything before the [`BuildKey`]
/// its name ends with.
///
/// A name this crate makes is `<sha>-<build key>`, and a build key is hex, so
/// the last `-` splits the two. A name that is one bare sha is a release an
/// earlier version of this dog made, or one an operator linked by hand, and
/// names itself.
#[must_use]
pub fn release_sha(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((sha, key)) if is_build_key(key) => sha,
        _ => name,
    }
}

/// Whether `text` is a [`BuildKey`] as it is spelled in a release name.
fn is_build_key(text: &str) -> bool {
    text.len() == BUILD_KEY_WIDTH && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether a directory name under `releases/` is one of this crate's releases:
/// a full git object id, with or without the build key this crate now appends.
#[must_use]
pub fn is_release_name(name: &str) -> bool {
    is_sha(release_sha(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fixtures;

    /// fails if two sheep that build the same way get different keys, or two
    /// that build differently get the same one. The key is the whole reuse
    /// rule: a miss rebuilds a release somebody already has, and a false hit
    /// serves one sheep another sheep's build output.
    #[test]
    fn a_build_key_is_the_command_the_user_and_the_build_env() {
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<String, String>>()
        };
        let key = |command: Option<&str>, user: Option<&str>, env: &BTreeMap<String, String>| {
            BuildKey::of(command, user, env).as_str().to_owned()
        };

        let none = env(&[]);
        let same = key(Some("make"), Some("reactmap"), &none);
        assert_eq!(key(Some("make"), Some("reactmap"), &none), same);
        assert_eq!(
            key(Some("make"), Some("reactmap"), &env(&[("A", "1")])).len(),
            16
        );

        for other in [
            key(None, Some("reactmap"), &none),
            key(Some("build"), Some("reactmap"), &none),
            key(Some("make"), None, &none),
            key(Some("make"), Some("other"), &none),
            key(Some("make"), Some("reactmap"), &env(&[("A", "1")])),
        ] {
            assert_ne!(other, same, "{other} is another build");
        }

        // Length-prefixed, so a value cannot be read as a different split of
        // the same bytes: `command = "a"` with `A = "c"` and `command = "ac"`
        // with no environment are the same string unprefixed.
        assert_ne!(
            key(Some("a"), None, &env(&[("b", "c")])),
            key(Some("ac"), None, &none)
        );
        assert_ne!(
            key(Some("a"), Some("b"), &env(&[("c", "d")])),
            key(Some("a"), Some("bc"), &env(&[("d", "")]))
        );
    }

    /// fails if a release name is read as anything other than the sha it was
    /// built from. Every caller that asks "which commit is this release" reads
    /// it off the directory name, and a rollback that misreads one writes a
    /// `deployed` that `State::read` then refuses forever.
    #[test]
    fn a_release_name_carries_its_sha_and_its_build_key() {
        let build = BuildKey::of(Some("make"), None, &BTreeMap::new());
        let name = format!("{}-{}", fixtures::SHA, build.as_str());

        assert_eq!(release_sha(&name), fixtures::SHA);
        assert!(is_release_name(&name));
        // A release an earlier version made, and one an operator linked by
        // hand, both name themselves.
        assert_eq!(release_sha(fixtures::SHA), fixtures::SHA);
        assert_eq!(release_sha("current"), "current");
        assert!(!is_release_name("notes.txt"));
        assert!(!is_release_name(&format!(
            "{}-zzzzzzzzzzzzzzzz",
            fixtures::SHA
        )));
    }
}
