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

    /// Every commit is on the default branch, so the local branch holds
    /// nothing of its own.
    pub fn on_default(&self) -> bool {
        self.ahead_of_default == 0
    }

    /// The work was pushed once and is now on the default branch: it landed.
    /// A branch that was never pushed and is on default was never started.
    pub fn landed(&self) -> bool {
        self.on_default() && !matches!(self.upstream, Upstream::None)
    }
}

/// The identifying fields every per-worktree report entry carries.
#[derive(Debug, Clone, Serialize)]
pub struct Subject {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
    pub head: Option<CommitSha>,
}

#[derive(Debug)]
pub struct Unchecked;

#[derive(Debug)]
pub struct Removable {
    basis: Basis,
    delete_branch: bool,
}

/// Why removal is allowed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum Basis {
    /// Clean, and every commit is on a remote branch (the upstream, or the
    /// default branch when there is no live upstream).
    NothingUnpushed,
    /// Clean and HEAD is exactly the head of a merged PR/MR, as the agent
    /// reported from the forge. Covers squash merges, where the commits never
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
    /// Commits on no remote branch: never pushed.
    Unpushed {
        commits: usize,
    },
    /// The remote branch was deleted and its commits are not on the default
    /// branch: usually a squash merge. `merge_check` says how to confirm.
    UpstreamGone {
        commits: usize,
    },
    /// A merged sha was given, but the worktree is somewhere else.
    HeadMoved {
        merged: CommitSha,
    },
    Locked,
}

#[derive(Debug)]
pub struct Worktree<S> {
    pub subject: Subject,
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
            subject: Subject {
                repo: found.repo.clone(),
                path: found.entry.path.clone(),
                branch: found.entry.branch.clone(),
                head: found.entry.head.clone(),
            },
            owner,
            locked: found.entry.locked,
            state: Unchecked,
        }
    }

    pub fn git(&self) -> Git {
        Git::at(&self.subject.path)
    }

    /// `merged` are the shas the agent reported as merged PR/MR heads for this
    /// repo. One that equals HEAD proves the work landed; one that does not
    /// is reported back, since the agent's information is stale.
    pub fn check(self, facts: &Facts, merged: &[&CommitSha]) -> Checked {
        let keep = if self.locked {
            Keep::Locked
        } else if !facts.changes.is_clean() {
            Keep::Uncommitted {
                changes: facts.changes.clone(),
            }
        } else if let Some(sha) = merged.iter().find(|s| facts.head.as_ref() == Some(**s)) {
            let basis = Basis::Merged {
                sha: (*sha).clone(),
            };
            return Checked::Removable(self.into_removable(basis, true));
        } else if facts.unpushed == 0 {
            let delete_branch = facts.on_default();
            return Checked::Removable(self.into_removable(Basis::NothingUnpushed, delete_branch));
        } else if let Some(sha) = merged.first() {
            Keep::HeadMoved {
                merged: (*sha).clone(),
            }
        } else if matches!(facts.upstream, Upstream::Gone { .. }) {
            Keep::UpstreamGone {
                commits: facts.unpushed,
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
        self.into_removable(Basis::Forced, true)
    }

    fn into_removable(self, basis: Basis, delete_branch: bool) -> Worktree<Removable> {
        Worktree {
            subject: self.subject,
            owner: self.owner,
            locked: self.locked,
            state: Removable {
                basis,
                delete_branch,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Removed {
    #[serde(flatten)]
    pub subject: Subject,
    #[serde(flatten)]
    pub basis: Basis,
    /// The local branch was deleted too. It stays when it has commits that
    /// are pushed but not on the default branch. ws never deletes remote
    /// branches.
    pub local_branch_deleted: bool,
}

impl Worktree<Removable> {
    pub fn basis(&self) -> &Basis {
        &self.state.basis
    }

    pub fn remove(self, lock: &Lock) -> Result<Removed, Error> {
        let Removable {
            basis,
            delete_branch,
        } = self.state;
        self.owner
            .worktree_remove(lock, &self.subject.path, matches!(basis, Basis::Forced))?;
        let local_branch_deleted = match (&self.subject.branch, delete_branch) {
            (Some(b), true) => {
                self.owner.branch_delete(lock, b)?;
                true
            }
            _ => false,
        };
        Ok(Removed {
            subject: self.subject,
            basis,
            local_branch_deleted,
        })
    }
}
