//! `ws prune`: find finished spaces. Reports by default; `--yes` removes the
//! spaces where every worktree is finished.
//!
//! Finished is stricter than what `ws drop` accepts. A pushed branch with an
//! open MR loses nothing when its worktree goes, but it is work in progress;
//! prune only clears work that landed.

use serde::Serialize;

use super::Report;
use super::drop::{
    DropReport, FetchError, Kept, MergedProof, Safety, evaluate, kept, refresh, remove_space,
};
use crate::error::{Error, Exit};
use crate::git::Upstream;
use crate::names::{BranchName, CommitSha, RepoName, SpaceName};
use crate::space;
use crate::workspace::Workspace;
use crate::worktree::{Basis, Checked, Facts};

#[derive(Debug, Serialize)]
pub struct PruneReport {
    pub applied: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fetch_errors: Vec<FetchError>,
    pub spaces: Vec<SpaceVerdict>,
}

#[derive(Debug, Serialize)]
pub struct SpaceVerdict {
    pub space: SpaceName,
    /// Every worktree is finished and there are no strays.
    pub finished: bool,
    pub worktrees: Vec<WorktreeVerdict>,
    pub strays: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<DropReport>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum WorktreeVerdict {
    Finished {
        repo: RepoName,
        #[serde(flatten)]
        basis: Basis,
    },
    /// Safe to drop, but not landed.
    Active {
        repo: RepoName,
        branch: Option<BranchName>,
        head: Option<CommitSha>,
        #[serde(flatten)]
        state: Active,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    Kept(Kept),
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Active {
    /// Pushed, with commits not on the default branch: an open MR, or a
    /// squash merge that only GitLab knows about.
    Unmerged { ahead_of_default: usize },
    /// Never pushed and nothing committed.
    Idle,
}

/// Landed: merged with proof, or pushed once and fully on the default branch.
fn finished(basis: &Basis, facts: &Facts) -> Option<Active> {
    match basis {
        Basis::Merged { .. } | Basis::Forced => None,
        Basis::Local => match (&facts.upstream, facts.ahead_of_default) {
            (Upstream::None, 0) => Some(Active::Idle),
            (_, 0) => None,
            (_, n) => Some(Active::Unmerged {
                ahead_of_default: n,
            }),
        },
    }
}

impl Report for PruneReport {
    fn exit(&self) -> Exit {
        let failed = self
            .spaces
            .iter()
            .filter_map(|s| s.dropped.as_ref())
            .any(|d| !d.space_removed);
        Exit::incomplete_if(failed)
    }
}

/// Whether prune removes what it finds removable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    DryRun,
    Remove,
}

pub fn prune(ws: &Workspace, proofs: &[MergedProof], apply: Apply) -> Result<PruneReport, Error> {
    let lock = ws.lock()?;
    let scans = space::scan_all(ws)?;
    let fetch_errors = refresh(ws, &lock, &scans.iter().collect::<Vec<_>>());
    let mut spaces = Vec::new();
    for scan in scans {
        let mut worktrees = Vec::new();
        for found in &scan.worktrees {
            let (checked, facts, _) = evaluate(ws, found, proofs)?;
            worktrees.push(match checked {
                Checked::Removable(wt) => match finished(wt.basis(), &facts) {
                    None => WorktreeVerdict::Finished {
                        repo: wt.repo.clone(),
                        basis: wt.basis().clone(),
                    },
                    Some(state) => {
                        let gitlab = &ws.roster.require(&found.repo)?.gitlab;
                        let hint = match (&state, &wt.branch, &facts.head) {
                            (Active::Unmerged { .. }, Some(b), Some(head)) => Some(format!(
                                "if the MR for {b} in {gitlab} is merged at {head}, pass --merged {}={head}",
                                wt.repo
                            )),
                            _ => None,
                        };
                        WorktreeVerdict::Active {
                            repo: wt.repo.clone(),
                            branch: wt.branch.clone(),
                            head: facts.head.clone(),
                            state,
                            hint,
                        }
                    }
                },
                Checked::Keep(wt, keep) => {
                    let gitlab = ws.roster.require(&found.repo)?.gitlab.to_string();
                    WorktreeVerdict::Kept(kept(&wt, &facts, keep, &gitlab))
                }
            });
        }
        let finished = scan.strays.is_empty()
            && worktrees
                .iter()
                .all(|w| matches!(w, WorktreeVerdict::Finished { .. }));
        let strays = scan.strays.len();
        let space = scan.name.clone();
        // Removal re-checks every worktree itself; the verdict above only
        // decides which spaces to try.
        let dropped = if finished && apply == Apply::Remove {
            Some(remove_space(ws, &lock, scan, proofs, Safety::Checked)?)
        } else {
            None
        };
        spaces.push(SpaceVerdict {
            space,
            finished,
            worktrees,
            strays,
            dropped,
        });
    }
    Ok(PruneReport {
        applied: apply == Apply::Remove,
        fetch_errors,
        spaces,
    })
}
