//! `ws prune`: find finished spaces. Reports by default; `--yes` removes the
//! spaces where every worktree is finished.
//!
//! Finished is stricter than what `ws drop` accepts. A pushed branch with an
//! open PR/MR loses nothing when its worktree goes, but it is work in progress;
//! prune only clears work that landed.

use std::path::PathBuf;

use serde::Serialize;

use super::Report;
use crate::error::{Error, Exit};
use crate::names::SpaceName;
use crate::removal::{
    Evaluated, FetchError, Kept, MergeCheck, MergedProof, Safety, SpaceRemoval, evaluate_all,
    refresh, remove_space,
};
use crate::space;
use crate::workspace::Workspace;
use crate::worktree::{Basis, Checked, Subject};

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
    pub strays: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removal: Option<SpaceRemoval>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum WorktreeVerdict {
    Finished {
        #[serde(flatten)]
        subject: Subject,
        #[serde(flatten)]
        basis: Basis,
    },
    /// Safe to drop, but not landed: prune leaves it.
    Active {
        #[serde(flatten)]
        subject: Subject,
        #[serde(flatten)]
        progress: Progress,
        #[serde(skip_serializing_if = "Option::is_none")]
        merge_check: Option<MergeCheck>,
    },
    Kept(Kept),
}

#[derive(Debug, Serialize)]
#[serde(tag = "progress", rename_all = "snake_case")]
pub enum Progress {
    /// Nothing committed beyond the default branch yet; just started.
    NoCommits,
    /// Pushed, with commits not on the default branch: an open PR/MR, or a
    /// squash merge that `merge_check` can confirm.
    Unmerged { ahead_of_default: usize },
}

impl WorktreeVerdict {
    fn of(e: &Evaluated) -> Self {
        match &e.checked {
            Checked::Removable(wt) => {
                let landed = matches!(wt.basis(), Basis::Merged { .. }) || e.facts.landed();
                if landed {
                    Self::Finished {
                        subject: wt.subject.clone(),
                        basis: wt.basis().clone(),
                    }
                } else {
                    let progress = if e.facts.on_default() {
                        Progress::NoCommits
                    } else {
                        Progress::Unmerged {
                            ahead_of_default: e.facts.ahead_of_default,
                        }
                    };
                    Self::Active {
                        subject: wt.subject.clone(),
                        progress,
                        merge_check: e.merge_check.clone(),
                    }
                }
            }
            Checked::Keep(wt, keep) => Self::Kept(Kept {
                subject: wt.subject.clone(),
                keep: keep.clone(),
                merge_check: e.merge_check.clone(),
            }),
        }
    }
}

impl Report for PruneReport {
    fn exit(&self) -> Exit {
        let failed = self
            .spaces
            .iter()
            .filter_map(|s| s.removal.as_ref())
            .any(|r| !r.space_removed);
        Exit::incomplete_if(failed)
    }
}

/// Whether prune removes what it finds finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    DryRun,
    Remove,
}

pub fn prune(ws: &Workspace, proofs: &[MergedProof], apply: Apply) -> Result<PruneReport, Error> {
    let lock = ws.lock()?;
    let scans = space::scan_all(ws)?;
    let fetch_errors = refresh(
        ws,
        &lock,
        scans
            .iter()
            .flat_map(|s| s.worktrees.iter().map(|f| &f.repo)),
    );
    let all: Vec<_> = scans.iter().flat_map(|s| &s.worktrees).collect();
    let mut evaluated = evaluate_all(ws, &all, proofs)?.into_iter();

    let mut spaces = Vec::new();
    for scan in scans {
        let mine: Vec<Evaluated> = evaluated.by_ref().take(scan.worktrees.len()).collect();
        let worktrees: Vec<WorktreeVerdict> = mine.iter().map(WorktreeVerdict::of).collect();
        let finished = scan.strays.is_empty()
            && worktrees
                .iter()
                .all(|w| matches!(w, WorktreeVerdict::Finished { .. }));
        // Removal acts on the same decisions, taken under the same lock.
        let removal = if finished && apply == Apply::Remove {
            Some(remove_space(
                ws,
                &lock,
                &scan.name,
                mine,
                scan.missing,
                false,
                Safety::Checked,
            )?)
        } else {
            None
        };
        spaces.push(SpaceVerdict {
            space: scan.name,
            finished,
            worktrees,
            strays: scan.strays,
            removal,
        });
    }
    Ok(PruneReport {
        applied: apply == Apply::Remove,
        fetch_errors,
        spaces,
    })
}
