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
use crate::warm::{self, WarmReport, Warmth};
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warm: Vec<WarmReport>,
}

/// Warm build dirs for a placed worktree, unless it failed or `--cold`.
pub(crate) fn warm_placed(
    ws: &Workspace,
    repo: &Repo<'_>,
    path: &Path,
    outcome: &CreateOutcome,
    warmth: Warmth,
) -> Vec<WarmReport> {
    // Existing worktrees are in use; copying under a running build is not
    // what anyone wants.
    if warmth == Warmth::Cold
        || matches!(
            outcome,
            CreateOutcome::Failed(_) | CreateOutcome::Existing { .. }
        )
    {
        return Vec::new();
    }
    warm::warm(repo, &ws.config.warm, path)
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CreateOutcome {
    /// New branch from the default branch on origin, or from the local HEAD
    /// of `repos/<repo>` when origin has none.
    Created {
        /// `origin/<default>`, or `HEAD` of `repos/<repo>` when origin has
        /// no default branch.
        base: String,
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

pub fn spike(
    ws: &Workspace,
    name: SpaceName,
    repos: &[RepoName],
    warmth: Warmth,
) -> Result<CreateReport, Error> {
    create(
        ws,
        &Work::Spike(name),
        Meta::new(Purpose::Spike),
        repos,
        warmth,
    )
}

pub fn start(
    ws: &Workspace,
    key: JiraKey,
    issue: IssueType,
    change: Option<ChangeType>,
    parent: Option<JiraKey>,
    repos: &[RepoName],
    warmth: Warmth,
) -> Result<CreateReport, Error> {
    let change = resolve(issue, change)?;
    let meta = Meta::new(Purpose::Ticket {
        ticket: key.clone(),
        issue,
        change,
        parent,
    });
    create(ws, &Work::Ticket { key, change }, meta, repos, warmth)
}

fn create(
    ws: &Workspace,
    work: &Work,
    meta: Meta,
    repos: &[RepoName],
    warmth: Warmth,
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
            warm: Vec::new(),
            repo: repo.entry.name.clone(),
            path,
            branch,
            outcome,
            fetch_error,
        }
    });

    let notes_created = space::scaffold(&dir, &space, &meta)?;
    // Warm copies only write ignored dirs inside the new worktrees, and a
    // full copy can take minutes: other agents should not wait for it.
    drop(lock);
    let mut worktrees = worktrees;
    for (wt, repo) in worktrees.iter_mut().zip(&repos) {
        wt.warm = warm_placed(ws, repo, &wt.path, &wt.outcome, warmth);
    }
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
    let (base, shown) = if repo.git.has(Rev::Origin(&default))? {
        (Rev::Origin(&default), format!("origin/{default}"))
    } else {
        (Rev::Head, "HEAD".to_owned())
    };
    repo.git.worktree_add(lock, path, branch, Some(base))?;
    Ok((CreateOutcome::Created { base: shown }, fetch_error))
}
