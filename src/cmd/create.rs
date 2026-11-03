//! `ws spike` and `ws start`: create a space with one worktree per repo.
//! Re-running is safe: existing worktrees are reported, missing ones added.

use std::path::PathBuf;

use serde::Serialize;

use super::{Defaults, Failure, Report};
use crate::branch::{ChangeType, IssueType, Work, resolve};
use crate::error::{Error, Exit, InputError, WorkspaceError};
use crate::names::{BranchName, JiraKey, RepoName, SpaceName};
use crate::space::{self, Meta};
use crate::workspace::{Repo, Workspace, canonical};

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
    Created {
        base: String,
    },
    /// The branch already existed locally and was checked out as is.
    ReusedBranch,
    /// The worktree was already there. `branch` is what it has checked out.
    Existing {
        checked_out: Option<BranchName>,
    },
    Failed(Failure),
}

impl Report for CreateReport {
    fn exit(&self) -> Exit {
        if self
            .worktrees
            .iter()
            .any(|w| matches!(w.outcome, CreateOutcome::Failed(_)))
        {
            Exit::Incomplete
        } else {
            Exit::Ok
        }
    }
}

pub fn spike(ws: &Workspace, name: SpaceName, repos: &[RepoName]) -> Result<CreateReport, Error> {
    let meta = Meta::Spike {
        created: space::today(),
    };
    create(ws, &Work::Spike(name), meta, repos)
}

pub fn start(
    ws: &Workspace,
    key: JiraKey,
    issue: IssueType,
    change: Option<ChangeType>,
    repos: &[RepoName],
) -> Result<CreateReport, Error> {
    let change = resolve(issue, change)?;
    let meta = Meta::Ticket {
        ticket: key.clone(),
        issue,
        change,
        created: space::today(),
    };
    create(ws, &Work::Ticket { key, change }, meta, repos)
}

fn create(
    ws: &Workspace,
    work: &Work,
    meta: Meta,
    repos: &[RepoName],
) -> Result<CreateReport, Error> {
    let _lock = ws.lock()?;
    // Everything that can be checked is checked before the first change.
    let repos: Vec<Repo<'_>> = repos
        .iter()
        .map(|r| ws.cloned(r))
        .collect::<Result<_, _>>()?;
    let space = work.space();
    let dir = ws.space_dir(&space);
    if let Some(existing) = space::read_meta(&dir)?
        && !existing.same_work(&meta)
    {
        return Err(InputError::SpaceConflict {
            space,
            existing: Box::new(existing),
        }
        .into());
    }
    std::fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
    let registered = space::registered(ws)?;
    let mut defaults = Defaults::default();

    let mut worktrees = Vec::new();
    for repo in &repos {
        let path = dir.join(&repo.entry.name);
        let branch = work.branch(repo.entry.style);
        let mut fetch_error = None;
        let outcome = if path.exists() {
            match registered.get(&canonical(&path)) {
                Some(f) if f.repo == repo.entry.name => CreateOutcome::Existing {
                    checked_out: f.entry.branch.clone(),
                },
                _ => CreateOutcome::Failed(
                    Error::from(WorkspaceError::Occupied {
                        repo: repo.entry.name.clone(),
                        path: path.clone(),
                    })
                    .into(),
                ),
            }
        } else {
            // A failed fetch is not fatal: the worktree starts from the last
            // known state and the report says so.
            if let Err(e) = repo.git.fetch() {
                fetch_error = Some(e.to_string());
            }
            add_worktree(repo, &mut defaults, &path, &branch)
                .unwrap_or_else(|e| CreateOutcome::Failed(e.into()))
        };
        worktrees.push(WorktreeResult {
            repo: repo.entry.name.clone(),
            path,
            branch,
            outcome,
            fetch_error,
        });
    }

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

fn add_worktree(
    repo: &Repo<'_>,
    defaults: &mut Defaults,
    path: &std::path::Path,
    branch: &BranchName,
) -> Result<CreateOutcome, Error> {
    if repo.git.ref_exists(&format!("refs/heads/{branch}"))? {
        repo.git.worktree_add_existing(path, branch)?;
        return Ok(CreateOutcome::ReusedBranch);
    }
    let default = defaults.of(repo)?;
    let remote = format!("origin/{default}");
    let base = if repo.git.ref_exists(&format!("refs/remotes/{remote}"))? {
        remote
    } else {
        "HEAD".to_owned()
    };
    repo.git.worktree_add_new(path, branch, &base)?;
    Ok(CreateOutcome::Created { base })
}
