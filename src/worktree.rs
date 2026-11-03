//! Worktree facts, and removing worktrees safely.
//!
//! A worktree can only be removed as `Worktree<Removable>`, and the only ways
//! to get one are `check` or an explicit `force`. So no code path can delete
//! work without having looked at it first.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::Error;
use crate::git::{Changes, Git, Rev, Untracked, Upstream};
use crate::names::{BranchName, CommitSha, RepoName};
use crate::space::Found;
use crate::workspace::Lock;

/// What git says about a worktree, measured against the repo's default branch.
#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    pub head: Option<CommitSha>,
    pub changes: Changes,
    pub upstream: Upstream,
    /// Commits not on the upstream, or not on the default branch when there
    /// is no live upstream. Without that fallback a never-pushed branch would
    /// count as fully pushed.
    pub unpushed: usize,
    pub ahead_of_default: usize,
}

impl Facts {
    pub fn read(git: &Git, default: &BranchName) -> Result<Self, Error> {
        let status = git.status(Untracked::Count)?;
        let (ahead_of_default, _) = git.ahead_behind(Rev::Origin(default))?;
        let unpushed = match &status.upstream {
            Upstream::Set { ahead, .. } => *ahead,
            Upstream::None | Upstream::Gone { .. } => ahead_of_default,
        };
        Ok(Self {
            head: status.head,
            changes: status.changes,
            upstream: status.upstream,
            unpushed,
            ahead_of_default,
        })
    }
}

#[derive(Debug)]
pub struct Unchecked;

#[derive(Debug)]
pub struct Removable(Basis);

/// Why removal is allowed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum Basis {
    /// Clean and nothing unpushed.
    Local,
    /// Clean and HEAD is exactly the head of a merged MR, as the agent
    /// reported from GitLab. Covers squash merges, where the commits never
    /// reach the default branch.
    Merged {
        sha: CommitSha,
    },
    Forced,
}

/// Why a worktree stays.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Keep {
    Uncommitted {
        changes: Changes,
    },
    Unpushed {
        commits: usize,
    },
    /// A merged sha was given, but the worktree is somewhere else.
    HeadMoved {
        merged: CommitSha,
        head: Option<CommitSha>,
    },
    Locked,
}

#[derive(Debug)]
pub struct Worktree<S> {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
    /// The canonical repo the worktree belongs to.
    owner: Git,
    locked: bool,
    state: S,
}

pub enum Checked {
    Removable(Worktree<Removable>),
    Keep(Worktree<Unchecked>, Keep),
}

impl Worktree<Unchecked> {
    pub fn new(found: &Found, owner: Git) -> Self {
        Self {
            repo: found.repo.clone(),
            path: found.entry.path.clone(),
            branch: found.entry.branch.clone(),
            owner,
            locked: found.entry.locked,
            state: Unchecked,
        }
    }

    pub fn git(&self) -> Git {
        Git::at(&self.path)
    }

    pub fn check(self, facts: &Facts, merged: Option<&CommitSha>) -> Checked {
        let keep = if self.locked {
            Keep::Locked
        } else if !facts.changes.is_clean() {
            Keep::Uncommitted {
                changes: facts.changes,
            }
        } else if let Some(sha) = merged.filter(|s| facts.head.as_ref() == Some(*s)) {
            return Checked::Removable(self.into_removable(Basis::Merged { sha: sha.clone() }));
        } else if facts.unpushed == 0 {
            return Checked::Removable(self.into_removable(Basis::Local));
        } else if let Some(sha) = merged {
            Keep::HeadMoved {
                merged: sha.clone(),
                head: facts.head.clone(),
            }
        } else {
            Keep::Unpushed {
                commits: facts.unpushed,
            }
        };
        Checked::Keep(self, keep)
    }

    /// Skips every check. Uncommitted changes and unpushed commits are lost.
    pub fn force(self) -> Worktree<Removable> {
        self.into_removable(Basis::Forced)
    }

    fn into_removable(self, basis: Basis) -> Worktree<Removable> {
        Worktree {
            repo: self.repo,
            path: self.path,
            branch: self.branch,
            owner: self.owner,
            locked: self.locked,
            state: Removable(basis),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Removed {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
    #[serde(flatten)]
    pub basis: Basis,
    /// The local branch was deleted too. It stays when it has commits that
    /// are pushed but not on the default branch.
    pub branch_deleted: bool,
}

impl Worktree<Removable> {
    pub fn remove(self, lock: &Lock, default: &BranchName) -> Result<Removed, Error> {
        let basis = self.state.0;
        self.owner
            .worktree_remove(lock, &self.path, matches!(basis, Basis::Forced))?;
        self.owner.worktree_prune(lock)?;
        let branch_deleted = match &self.branch {
            None => false,
            Some(b) => {
                let delete = match &basis {
                    Basis::Merged { .. } | Basis::Forced => true,
                    Basis::Local => self
                        .owner
                        .is_ancestor(Rev::Local(b), Rev::Origin(default))?,
                };
                if delete {
                    self.owner.branch_delete(lock, b)?;
                }
                delete
            }
        };
        Ok(Removed {
            repo: self.repo,
            path: self.path,
            branch: self.branch,
            basis,
            branch_deleted,
        })
    }
}
