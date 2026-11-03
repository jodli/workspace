//! `ws spike` and `ws start`: create a space with one worktree per repo.
//! Re-running is safe: existing worktrees are reported, missing ones added.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Failure, Report};
use crate::branch::{ChangeType, IssueType, Work, resolve};
use crate::error::{Error, Exit, InputError, WorkspaceError};
use crate::git::Rev;
use crate::names::{BranchName, JiraKey, RepoName, SpaceName};
use crate::space::{self, Meta, Purpose};
use crate::workspace::{Lock, Repo, Slot, Workspace, par_map};

#[derive(Debug, Serialize)]
pub struct CreateReport {
    pub space: SpaceName,
    pub path: PathBuf,
    pub meta: Meta,
    pub notes: PathBuf,
    pub notes_created: bool,
    pub worktrees: Vec<WorktreeResult>,
}

#[derive(Debug, Serialize)]
pub struct WorktreeResult {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: BranchName,
    #[serde(flatten)]
    pub outcome: CreateOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CreateOutcome {
    /// New branch from the default branch on origin, or from the local HEAD
    /// of `repos/<repo>` when origin has none.
    Created {
        from_origin: bool,
    },
    /// The branch already existed locally and was checked out as is.
    ReusedBranch,
    /// The worktree was already there, with this branch checked out.
    Existing {
        checked_out: Option<BranchName>,
    },
    Failed(Failure),
}

impl Report for CreateReport {
    fn exit(&self) -> Exit {
        Exit::incomplete_if(
            self.worktrees
                .iter()
                .any(|w| matches!(w.outcome, CreateOutcome::Failed(_))),
        )
    }
}

pub fn spike(ws: &Workspace, name: SpaceName, repos: &[RepoName]) -> Result<CreateReport, Error> {
    create(ws, &Work::Spike(name), Meta::new(Purpose::Spike), repos)
}

pub fn start(
    ws: &Workspace,
    key: JiraKey,
    issue: IssueType,
    change: Option<ChangeType>,
    repos: &[RepoName],
) -> Result<CreateReport, Error> {
    let change = resolve(issue, change)?;
    let meta = Meta::new(Purpose::Ticket {
        ticket: key.clone(),
        issue,
        change,
    });
    create(ws, &Work::Ticket { key, change }, meta, repos)
}

fn create(
    ws: &Workspace,
    work: &Work,
    meta: Meta,
    repos: &[RepoName],
) -> Result<CreateReport, Error> {
    let lock = ws.lock()?;
    // Everything that can be checked is checked before the first change.
    let repos: Vec<Repo<'_>> = repos
        .iter()
        .map(|r| ws.cloned(r))
        .collect::<Result<_, _>>()?;
    let space = work.space();
    let dir = ws.space_dir(&space);
    if let Some(existing) = space::read_meta(&dir)?
        && existing.purpose != meta.purpose
    {
        return Err(InputError::SpaceConflict {
            space,
            existing: Box::new(existing),
        }
        .into());
    }
    std::fs::create_dir_all(&dir).map_err(Error::io(&dir))?;

    let worktrees = par_map(&repos, |repo| {
        let path = dir.join(&repo.entry.name);
        let branch = work.branch(repo.entry.style);
        let (outcome, fetch_error) = place(&lock, repo, &path, &branch, Fetch::Yes)
            .unwrap_or_else(|e| (CreateOutcome::Failed(e.into()), None));
        WorktreeResult {
            repo: repo.entry.name.clone(),
            path,
            branch,
            outcome,
            fetch_error,
        }
    });

    let notes_created = space::scaffold(&dir, &space, &meta)?;
    Ok(CreateReport {
        notes: dir.join(space::NOTES_FILE),
        space,
        path: dir,
        meta,
        notes_created,
        worktrees,
    })
}

/// Whether `place` fetches before adding a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetch {
    Yes,
    /// The caller fetched already.
    Done,
}

/// Puts `branch` of `repo` at `path` unless it is there already. Shared with
/// `adopt`, which passes a branch that must exist.
pub(crate) fn place(
    lock: &Lock,
    repo: &Repo<'_>,
    path: &Path,
    branch: &BranchName,
    fetch: Fetch,
) -> Result<(CreateOutcome, Option<String>), Error> {
    match repo.slot(path)? {
        Slot::Ours(entry) => {
            return Ok((
                CreateOutcome::Existing {
                    checked_out: entry.branch,
                },
                None,
            ));
        }
        Slot::Occupied => {
            return Err(WorkspaceError::Occupied {
                repo: repo.entry.name.clone(),
                path: path.to_owned(),
            }
            .into());
        }
        Slot::Free => {}
    }
    // A failed fetch is not fatal: the worktree starts from the last known
    // state and the report says so.
    let fetch_error = match fetch {
        Fetch::Yes => repo.git.fetch(lock).err().map(|e| e.to_string()),
        Fetch::Done => None,
    };
    if repo.git.has(Rev::Local(branch))? {
        repo.git.worktree_add(lock, path, branch, None)?;
        return Ok((CreateOutcome::ReusedBranch, fetch_error));
    }
    let default = repo.default_branch()?;
    let from_origin = repo.git.has(Rev::Origin(&default))?;
    let base = if from_origin {
        Rev::Origin(&default)
    } else {
        Rev::Head
    };
    repo.git.worktree_add(lock, path, branch, Some(base))?;
    Ok((CreateOutcome::Created { from_origin }, fetch_error))
}
