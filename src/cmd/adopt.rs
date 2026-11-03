//! `ws adopt <space> <repo> <branch>`: put an existing branch into a space,
//! e.g. to review a PR/MR or to rescue work from `repos/`.

use serde::Serialize;

use super::Report;
use super::create::{CreateOutcome, Fetch, WorktreeResult, place, warm_placed};
use crate::error::{Error, Exit, WorkspaceError};
use crate::git::{Rev, Untracked};
use crate::names::{BranchName, RepoName, SpaceName};
use crate::space::{self, Meta, Purpose};
use crate::warm::Warmth;
use crate::workspace::{Slot, Workspace};

#[derive(Debug, Serialize)]
pub struct AdoptReport {
    pub space: SpaceName,
    pub worktree: WorktreeResult,
    /// `repos/<repo>` had the branch checked out and was put back on this one.
    pub restored_default: Option<BranchName>,
    pub notes_created: bool,
}

impl Report for AdoptReport {
    fn exit(&self) -> Exit {
        Exit::incomplete_if(matches!(self.worktree.outcome, CreateOutcome::Failed(_)))
    }
}

pub fn adopt(
    ws: &Workspace,
    space: SpaceName,
    repo: &RepoName,
    branch: BranchName,
    warmth: Warmth,
) -> Result<AdoptReport, Error> {
    let lock = ws.lock()?;
    let repo = ws.cloned(repo)?;
    let dir = ws.space_dir(&space);
    let path = dir.join(&repo.entry.name);

    let mut restored_default = None;
    let mut fetch_error = None;
    if matches!(repo.slot(&path)?, Slot::Free) {
        // The branch may only exist on origin so far.
        fetch_error = repo.git.fetch(&lock).err().map(|e| e.to_string());
        if !repo.git.has(Rev::Local(&branch))? {
            if !repo.git.has(Rev::Origin(&branch))? {
                return Err(WorkspaceError::NoSuchBranch {
                    repo: repo.entry.name.clone(),
                    branch,
                }
                .into());
            }
            repo.git.branch_track(&lock, &branch)?;
        }
        // A branch checked out in repos/ cannot also live in a worktree, and
        // repos/ belongs on its default branch anyway.
        let status = repo.git.status(Untracked::Skip)?;
        if status.branch.as_ref() == Some(&branch) {
            if status.changes.tracked > 0 {
                return Err(WorkspaceError::CheckedOutDirty {
                    repo: repo.entry.name.clone(),
                    branch,
                }
                .into());
            }
            let default = repo.default_branch()?;
            repo.git.checkout(&lock, &default)?;
            restored_default = Some(default);
        }
        std::fs::create_dir_all(&dir).map_err(Error::io(&dir))?;
    }

    let (outcome, _) = place(&lock, &repo, &path, &branch, Fetch::Done)?;
    let notes_created = space::scaffold(
        &dir,
        &space,
        &Meta::new(Purpose::Adopted {
            branch: branch.clone(),
        }),
    )?;
    drop(lock);
    let warm = warm_placed(ws, &repo, &path, &outcome, warmth);
    Ok(AdoptReport {
        space,
        worktree: WorktreeResult {
            warm,
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
