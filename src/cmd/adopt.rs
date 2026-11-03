//! `ws adopt <space> <repo> <branch>`: put an existing branch into a space,
//! e.g. to review an MR or to rescue work from `repos/`.

use serde::Serialize;

use super::Report;
use super::create::{CreateOutcome, WorktreeResult};
use crate::error::{Error, Exit, WorkspaceError};
use crate::names::{BranchName, RepoName, SpaceName};
use crate::space::{self, Meta};
use crate::workspace::{Workspace, canonical};

#[derive(Debug, Serialize)]
pub struct AdoptReport {
    pub space: SpaceName,
    pub worktree: WorktreeResult,
    /// `repos/<repo>` had the branch checked out and was put back on default.
    pub restored_default: Option<BranchName>,
    pub notes_created: bool,
}

impl Report for AdoptReport {
    fn exit(&self) -> Exit {
        if matches!(self.worktree.outcome, CreateOutcome::Failed(_)) {
            Exit::Incomplete
        } else {
            Exit::Ok
        }
    }
}

pub fn adopt(
    ws: &Workspace,
    space: SpaceName,
    repo: &RepoName,
    branch: BranchName,
) -> Result<AdoptReport, Error> {
    let _lock = ws.lock()?;
    let repo = ws.cloned(repo)?;
    let dir = ws.space_dir(&space);
    let path = dir.join(&repo.entry.name);

    if path.exists() {
        let outcome = match space::registered(ws)?.get(&canonical(&path)) {
            Some(f) if f.repo == repo.entry.name => CreateOutcome::Existing {
                checked_out: f.entry.branch.clone(),
            },
            _ => {
                return Err(WorkspaceError::Occupied {
                    repo: repo.entry.name.clone(),
                    path,
                }
                .into());
            }
        };
        return Ok(AdoptReport {
            space,
            worktree: WorktreeResult {
                repo: repo.entry.name.clone(),
                path,
                branch,
                outcome,
                fetch_error: None,
            },
            restored_default: None,
            notes_created: false,
        });
    }

    let fetch_error = repo.git.fetch().err().map(|e| e.to_string());
    let default = repo.git.default_branch()?;

    if !repo.git.ref_exists(&format!("refs/heads/{branch}"))? {
        if repo
            .git
            .ref_exists(&format!("refs/remotes/origin/{branch}"))?
        {
            repo.git.branch_track(&branch)?;
        } else {
            return Err(WorkspaceError::NoSuchBranch {
                repo: repo.entry.name.clone(),
                branch,
            }
            .into());
        }
    }

    // A branch checked out in repos/ cannot also live in a worktree, and
    // repos/ belongs on its default branch anyway.
    let mut restored_default = None;
    if repo.git.head_branch()?.as_ref() == Some(&branch) {
        if repo.git.changes()?.tracked > 0 {
            return Err(WorkspaceError::CheckedOutDirty {
                repo: repo.entry.name.clone(),
                branch,
            }
            .into());
        }
        repo.git.checkout(&default)?;
        restored_default = Some(default);
    }

    std::fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
    let outcome = match repo.git.worktree_add_existing(&path, &branch) {
        Ok(()) => CreateOutcome::ReusedBranch,
        Err(e) => CreateOutcome::Failed(Error::from(e).into()),
    };
    let meta = Meta::Adopted {
        branch: branch.clone(),
        created: space::today(),
    };
    let notes_created = space::scaffold(&dir, &space, &meta)?;
    Ok(AdoptReport {
        space,
        worktree: WorktreeResult {
            repo: repo.entry.name.clone(),
            path,
            branch,
            outcome,
            fetch_error,
        },
        restored_default,
        notes_created,
    })
}
