//! From a Jira issue type to a branch name.
//!
//! The agent reads the issue in Jira and passes its type; this module owns the
//! rule of what that type means for the branch.

use serde::{Deserialize, Serialize};

use crate::error::InputError;
use crate::names::{BranchName, JiraKey, SpaceName};

/// Jira issue types as the agent reports them. `SubTask` and `Epic` exist so
/// they can be refused with a precise hint instead of a generic parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum IssueType {
    Bug,
    Story,
    Task,
    SubTask,
    Epic,
}

/// Conventional commit types. The branch prefix is derived from these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeType {
    Feat,
    Fix,
    Chore,
    Refactor,
    Docs,
    Test,
    Ci,
    Perf,
    Build,
}

impl ChangeType {
    /// The conventional commit name. The one place these names are spelled.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feat => "feat",
            Self::Fix => "fix",
            Self::Chore => "chore",
            Self::Refactor => "refactor",
            Self::Docs => "docs",
            Self::Test => "test",
            Self::Ci => "ci",
            Self::Perf => "perf",
            Self::Build => "build",
        }
    }
}

/// Branch prefix convention of a repo, set per repo in the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefixStyle {
    /// `feat/`, `fix/`, `chore/`, ...
    #[default]
    Conventional,
    /// `feature/` and `bugfix/` for feat and fix, conventional names otherwise.
    Gitflow,
}

impl PrefixStyle {
    pub fn prefix(self, change: ChangeType) -> &'static str {
        match (self, change) {
            (Self::Gitflow, ChangeType::Feat) => "feature",
            (Self::Gitflow, ChangeType::Fix) => "bugfix",
            (_, c) => c.as_str(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conventional => "conventional",
            Self::Gitflow => "gitflow",
        }
    }
}

impl std::str::FromStr for PrefixStyle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [Self::Conventional, Self::Gitflow]
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| format!("unknown style '{s}', expected conventional or gitflow"))
    }
}

impl Serialize for PrefixStyle {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Decides the change type for a ticket. Bugs are fixes and stories are
/// features, no matter what; a task is engineering work whose type only the
/// agent can judge, so it must say.
pub fn resolve(issue: IssueType, change: Option<ChangeType>) -> Result<ChangeType, InputError> {
    let implied = match issue {
        IssueType::Bug => ChangeType::Fix,
        IssueType::Story => ChangeType::Feat,
        IssueType::Task => return change.ok_or(InputError::ChangeRequired),
        IssueType::SubTask => return Err(InputError::SubTask),
        IssueType::Epic => return Err(InputError::Epic),
    };
    match change {
        Some(given) if given != implied => Err(InputError::ChangeConflict {
            issue,
            implied,
            given,
        }),
        _ => Ok(implied),
    }
}

/// What a space is for. Decides the branch name in every repo of the space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    Spike(SpaceName),
    Ticket { key: JiraKey, change: ChangeType },
}

impl Work {
    pub fn space(&self) -> SpaceName {
        match self {
            Self::Spike(name) => name.clone(),
            Self::Ticket { key, .. } => SpaceName::from(key),
        }
    }

    pub fn branch(&self, style: PrefixStyle) -> BranchName {
        match self {
            Self::Spike(name) => BranchName::prefixed("spike", name),
            Self::Ticket { key, change } => BranchName::prefixed(style.prefix(*change), key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChangeType::{Chore, Feat, Fix};
    use IssueType::{Bug, Epic, Story, SubTask, Task};

    #[test]
    fn resolve_table() {
        let ok = [
            (Bug, None, Fix),
            (Bug, Some(Fix), Fix),
            (Story, None, Feat),
            (Story, Some(Feat), Feat),
            (Task, Some(Chore), Chore),
            (Task, Some(Feat), Feat),
        ];
        for (issue, change, want) in ok {
            assert!(
                matches!(resolve(issue, change), Ok(c) if c == want),
                "{issue:?} {change:?}"
            );
        }

        assert!(matches!(
            resolve(Task, None),
            Err(InputError::ChangeRequired)
        ));
        assert!(matches!(
            resolve(Bug, Some(Feat)),
            Err(InputError::ChangeConflict { .. })
        ));
        assert!(matches!(
            resolve(Story, Some(Chore)),
            Err(InputError::ChangeConflict { .. })
        ));
        assert!(matches!(
            resolve(SubTask, Some(Feat)),
            Err(InputError::SubTask)
        ));
        assert!(matches!(resolve(Epic, None), Err(InputError::Epic)));
    }

    #[test]
    fn branch_per_style() -> Result<(), InputError> {
        let work = Work::Ticket {
            key: "DEMO-1".parse()?,
            change: Fix,
        };
        assert_eq!(
            work.branch(PrefixStyle::Conventional).as_str(),
            "fix/DEMO-1"
        );
        assert_eq!(work.branch(PrefixStyle::Gitflow).as_str(), "bugfix/DEMO-1");
        let spike = Work::Spike("dep-audit".parse()?);
        assert_eq!(
            spike.branch(PrefixStyle::Gitflow).as_str(),
            "spike/dep-audit"
        );
        Ok(())
    }
}
