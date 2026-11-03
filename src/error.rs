//! Every failure an agent can see. Each error has a stable `kind`, an exit
//! code by category, and where possible a hint naming the fix.

use std::borrow::Cow;
use std::path::PathBuf;

use serde::Serialize;

use crate::branch::{ChangeType, IssueType};
use crate::names::{RepoName, SpaceName};

/// Process exit codes. 0 and 1 come with a normal result on stdout, the rest
/// with an error object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    /// Ran, but something was kept or failed. Details are in the result.
    Incomplete = 1,
    Input = 2,
    Workspace = 3,
    Git = 4,
    Busy = 5,
    Io = 6,
}

impl From<Exit> for std::process::ExitCode {
    fn from(e: Exit) -> Self {
        Self::from(e as u8)
    }
}

/// Bad input from the caller. Fixable by calling differently.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("invalid {what} '{value}': {rule}")]
    InvalidName {
        what: &'static str,
        value: String,
        rule: &'static str,
    },
    #[error("a task needs an explicit change type")]
    ChangeRequired,
    #[error("a {issue:?} is always {implied:?}, got --change {given:?}")]
    ChangeConflict {
        issue: IssueType,
        implied: ChangeType,
        given: ChangeType,
    },
    #[error("sub-tasks have no type of their own")]
    SubTask,
    #[error("an epic is not worked on directly")]
    Epic,
    #[error("invalid --merged '{0}'")]
    MergedProof(String),
    #[error("'{0}' is not in the roster")]
    UnknownRepo(RepoName),
    #[error("space '{space}' already exists as {existing}")]
    SpaceConflict { space: String, existing: String },
    #[error("{0}")]
    Usage(String),
}

impl InputError {
    fn kind(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "input.invalid_name",
            Self::ChangeRequired => "input.change_required",
            Self::ChangeConflict { .. } => "input.change_conflict",
            Self::SubTask => "input.sub_task",
            Self::Epic => "input.epic",
            Self::MergedProof(_) => "input.merged_proof",
            Self::UnknownRepo(_) => "input.unknown_repo",
            Self::SpaceConflict { .. } => "input.space_conflict",
            Self::Usage(_) => "input.usage",
        }
    }

    fn hint(&self) -> Option<Cow<'static, str>> {
        Some(match self {
            Self::InvalidName { .. } | Self::Usage(_) => return None,
            Self::ChangeRequired => {
                let names: Vec<_> = <ChangeType as clap::ValueEnum>::value_variants()
                    .iter()
                    .map(|c| c.as_str())
                    .collect();
                format!(
                    "judge the change from the ticket and pass --change <{}>",
                    names.join("|")
                )
                .into()
            }
            Self::ChangeConflict { issue, implied, .. } => {
                format!("drop --change; a {issue:?} is always {}", implied.as_str()).into()
            }
            Self::SubTask => "pass --issue with the type of the parent ticket".into(),
            Self::Epic => "start one of the epic's child tickets instead".into(),
            Self::MergedProof(_) => {
                "use --merged <repo>=<sha>, where sha is the full head sha of the merged MR".into()
            }
            Self::UnknownRepo(_) => {
                "`ws list` shows the roster; add a repo with `ws add <gitlab path>`".into()
            }
            Self::SpaceConflict { .. } => {
                "`ws status <space>` shows what the space is; pick another name".into()
            }
        })
    }
}

/// The workspace itself is missing or malformed.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("no workspace found from {0} (no ws.toml in it or any parent)")]
    NotFound(PathBuf),
    #[error("{path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("{path}:{line}: {message}")]
    Roster {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("no space '{0}'")]
    NoSuchSpace(SpaceName),
    #[error("repo '{repo}' is in the roster but not cloned at {path}")]
    RepoMissing { repo: RepoName, path: PathBuf },
}

impl WorkspaceError {
    fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "workspace.not_found",
            Self::Config { .. } => "workspace.config",
            Self::Roster { .. } => "workspace.roster",
            Self::NoSuchSpace(_) => "workspace.no_such_space",
            Self::RepoMissing { .. } => "workspace.repo_missing",
        }
    }

    // Same shape as the other categories, even though every variant has one.
    #[allow(clippy::unnecessary_wraps)]
    fn hint(&self) -> Option<Cow<'static, str>> {
        Some(match self {
            Self::NotFound(_) => "run inside a workspace or set WS_ROOT".into(),
            Self::Config { .. } => {
                "fix ws.toml; ws.example.toml in the ws repo lists every key".into()
            }
            Self::Roster { .. } => crate::roster::FORMAT.into(),
            Self::NoSuchSpace(_) => "`ws list` shows existing spaces".into(),
            Self::RepoMissing { .. } => "`ws sync` clones missing roster repos".into(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot run git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`git {args}` failed ({code}): {stderr}")]
    Failed {
        args: String,
        code: String,
        stderr: String,
    },
    #[error("`git {args}` printed something unexpected: {output}")]
    Unexpected { args: String, output: String },
}

impl GitError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "git.spawn",
            Self::Failed { .. } => "git.failed",
            Self::Unexpected { .. } => "git.unexpected",
        }
    }

    fn hint(&self) -> Option<Cow<'static, str>> {
        match self {
            Self::Spawn(_) => Some("install git and put it on PATH".into()),
            Self::Failed { .. } | Self::Unexpected { .. } => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Input(#[from] InputError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("another ws process holds {0}")]
    Busy(PathBuf),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }

    pub fn exit(&self) -> Exit {
        match self {
            Self::Input(_) => Exit::Input,
            Self::Workspace(_) => Exit::Workspace,
            Self::Git(_) => Exit::Git,
            Self::Io { .. } => Exit::Io,
            Self::Busy(_) => Exit::Busy,
        }
    }

    pub fn report(&self) -> ErrorReport {
        let (kind, hint) = match self {
            Self::Input(e) => (e.kind(), e.hint()),
            Self::Workspace(e) => (e.kind(), e.hint()),
            Self::Git(e) => (e.kind(), e.hint()),
            Self::Io { .. } => ("io", None),
            Self::Busy(_) => (
                "busy",
                Some("another agent is running ws here; retry in a few seconds".into()),
            ),
        };
        ErrorReport {
            kind,
            message: self.to_string(),
            hint,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorReport {
    pub kind: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<Cow<'static, str>>,
}
