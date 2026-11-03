//! `ws sync`: clone missing roster repos, fetch, fast-forward default branches.
//! A repo that is not exactly on a clean, not-ahead default branch is left
//! alone: work belongs in a space.

use serde::Serialize;

use super::{Defaults, Failure, Report};
use crate::error::{Error, Exit};
use crate::git::Git;
use crate::names::{BranchName, RepoName};
use crate::roster::RepoEntry;
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub repos: Vec<RepoSync>,
}

#[derive(Debug, Serialize)]
pub struct RepoSync {
    pub repo: RepoName,
    #[serde(flatten)]
    pub outcome: SyncOutcome,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SyncOutcome {
    Cloned,
    UpToDate { branch: BranchName },
    FastForwarded { branch: BranchName, commits: usize },
    Skipped { reason: SkipReason },
    Failed(Failure),
}

#[derive(Debug, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum SkipReason {
    NotOnDefault {
        branch: Option<BranchName>,
        default: BranchName,
    },
    Uncommitted,
    Ahead {
        commits: usize,
    },
}

impl Report for SyncReport {
    fn exit(&self) -> Exit {
        if self
            .repos
            .iter()
            .any(|r| matches!(r.outcome, SyncOutcome::Failed(_)))
        {
            Exit::Incomplete
        } else {
            Exit::Ok
        }
    }
}

pub fn sync(ws: &Workspace) -> Result<SyncReport, Error> {
    let _lock = ws.lock()?;
    sync_entries(ws, ws.roster.iter())
}

pub(crate) fn sync_entries<'w>(
    ws: &'w Workspace,
    entries: impl Iterator<Item = &'w RepoEntry>,
) -> Result<SyncReport, Error> {
    std::fs::create_dir_all(ws.repos_dir()).map_err(Error::io(ws.repos_dir()))?;
    let mut defaults = Defaults::default();
    let repos = entries
        .map(|entry| RepoSync {
            repo: entry.name.clone(),
            outcome: sync_one(ws, &mut defaults, entry)
                .unwrap_or_else(|e| SyncOutcome::Failed(e.into())),
        })
        .collect();
    Ok(SyncReport { repos })
}

fn sync_one(
    ws: &Workspace,
    defaults: &mut Defaults,
    entry: &RepoEntry,
) -> Result<SyncOutcome, Error> {
    let Some(repo) = ws.repo(entry) else {
        Git::clone_into(
            &ws.config.gitlab.ssh_url(&entry.gitlab),
            &ws.repo_dir(&entry.name),
        )?;
        return Ok(SyncOutcome::Cloned);
    };
    repo.git.fetch()?;
    let default = defaults.of(&repo)?;
    let branch = repo.git.head_branch()?;
    if branch.as_ref() != Some(&default) {
        return Ok(SyncOutcome::Skipped {
            reason: SkipReason::NotOnDefault { branch, default },
        });
    }
    // Untracked files do not block a fast-forward; git refuses by itself if
    // one would be overwritten.
    if repo.git.changes()?.tracked > 0 {
        return Ok(SyncOutcome::Skipped {
            reason: SkipReason::Uncommitted,
        });
    }
    let upstream = format!("origin/{default}");
    let ahead = repo.git.count_between(&upstream, "HEAD")?;
    if ahead > 0 {
        return Ok(SyncOutcome::Skipped {
            reason: SkipReason::Ahead { commits: ahead },
        });
    }
    let behind = repo.git.count_between("HEAD", &upstream)?;
    if behind == 0 {
        return Ok(SyncOutcome::UpToDate { branch: default });
    }
    repo.git.merge_ff_only(&upstream)?;
    Ok(SyncOutcome::FastForwarded {
        branch: default,
        commits: behind,
    })
}
