//! Validated names. Every string that reaches git or the filesystem as a path
//! segment or ref goes through one of these, so the rest of the code never
//! re-checks.

use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::error::InputError;

macro_rules! name_type {
    ($(#[$doc:meta])* $name:ident, $what:literal, $check:expr, $rule:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = InputError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let check: fn(&str) -> bool = $check;
                if check(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(InputError::InvalidName {
                        what: $what,
                        value: s.to_owned(),
                        rule: $rule,
                    })
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

/// A single path segment: starts alphanumeric, then `[A-Za-z0-9._-]`. Rules out
/// `.`, `..`, hidden dirs like `.archive`, separators and anything a shell or
/// Windows would trip over.
const SEGMENT_RULE: &str = "starts with a letter or digit, then letters, digits, '.', '_', '-'";

fn is_segment(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() <= 100
        && first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !s.ends_with(".lock")
}

name_type!(
    /// Directory name under `spaces/`. Ticket spaces use the ticket key verbatim.
    SpaceName,
    "space name",
    is_segment,
    SEGMENT_RULE
);

name_type!(
    /// Local name of a roster repo, the directory under `repos/`.
    RepoName,
    "repo name",
    is_segment,
    SEGMENT_RULE
);

name_type!(
    /// Project path on the GitLab host, e.g. `group/sub/project`.
    GitlabPath,
    "gitlab path",
    |s| s.split('/').all(is_segment),
    "segments of letters, digits, '.', '_', '-' separated by '/'"
);

name_type!(
    /// A ticket id from any tracker: a `PROJ-123` style key or a bare
    /// issue number like `123`. It names the space and ends the
    /// branch, so it must be a path segment; the digit keeps a ticket title
    /// from being passed by mistake.
    TicketKey,
    "ticket key",
    |s| is_segment(s) && s.chars().any(|c| c.is_ascii_digit()),
    "the tracker's ticket id, e.g. PROJ-123 or 123"
);

name_type!(
    /// A full commit id. Abbreviated ids are refused on purpose: the merged-MR
    /// check compares against HEAD and must not match by prefix.
    CommitSha,
    "commit sha",
    |s| matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
    "full lowercase hex commit id (40 or 64 characters)"
);

name_type!(
    /// A local branch name. Mirrors the parts of `git check-ref-format` that
    /// can be checked without git; git still has the final word.
    BranchName,
    "branch name",
    |s| {
        !s.is_empty()
            && !s.starts_with(['-', '/'])
            && !s.ends_with(['/', '.'])
            && !s.ends_with(".lock")
            && !s.contains("..")
            && !s.contains("//")
            && !s.contains("@{")
            && s != "HEAD"
            && !s.chars().any(|c| c.is_ascii_control() || " ~^:?*[\\".contains(c))
    },
    "a valid git branch name"
);

name_type!(
    /// A directory name inside a worktree, for the warm copy. Hidden dirs like
    /// `.venv` cannot be named: they tend to hold absolute paths.
    DirName,
    "directory name",
    is_segment,
    SEGMENT_RULE
);

name_type!(
    /// A host name, e.g. `gitlab.example.com`.
    Host,
    "host",
    |s| !s.is_empty()
        && !s.starts_with(['-', '.'])
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')),
    "letters, digits, '.', '-'"
);

/// Names that are valid as the last part of a branch: validated segments.
pub(crate) trait BranchLeaf {
    fn leaf(&self) -> &str;
}

impl BranchLeaf for SpaceName {
    fn leaf(&self) -> &str {
        &self.0
    }
}

impl BranchLeaf for TicketKey {
    fn leaf(&self) -> &str {
        &self.0
    }
}

impl BranchName {
    /// `prefix/name`. The prefix comes from a fixed table and the name is a
    /// validated segment, so the result is a valid branch name by construction.
    pub(crate) fn prefixed(prefix: &'static str, name: &impl BranchLeaf) -> Self {
        Self(format!("{prefix}/{}", name.leaf()))
    }
}

/// Path segments can be joined onto paths directly.
macro_rules! segment_path {
    ($($name:ident),*) => {$(
        impl AsRef<std::path::Path> for $name {
            fn as_ref(&self) -> &std::path::Path {
                std::path::Path::new(&self.0)
            }
        }
    )*};
}
segment_path!(SpaceName, RepoName, DirName);

impl DirName {
    /// For built-in defaults that are known to be valid.
    pub(crate) fn known(s: &'static str) -> Self {
        debug_assert!(is_segment(s));
        Self(s.to_owned())
    }
}

impl From<&TicketKey> for SpaceName {
    fn from(key: &TicketKey) -> Self {
        // A ticket key is a valid segment by construction.
        Self(key.0.clone())
    }
}
