//! `ws sync`: clone missing roster repos, fetch, fast-forward default branches.
//! A repo that is not exactly on a clean, not-ahead default branch is left
//! alone: work belongs in a space.

use serde::Serialize;

use super::{Failure, Report};
use crate::error::{Error, Exit};
use crate::git::{Git, Rev};
use crate::names::{BranchName, RepoName};
use crate::roster::RepoEntry;
use crate::workspace::{Canonical, Lock, Workspace, par_map};

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
    Skipped { canonical: Canonical },
    Failed(Failure),
}

impl Report for SyncReport {
    fn exit(&self) -> Exit {
        Exit::incomplete_if(
            self.repos
                .iter()
                .any(|r| matches!(r.outcome, SyncOutcome::Failed(_))),
        )
    }
}

pub fn sync(ws: &Workspace) -> Result<SyncReport, Error> {
    let lock = ws.lock()?;
    let entries: Vec<&RepoEntry> = ws.roster.iter().collect();
    sync_entries(ws, &lock, &entries)
}

pub(crate) fn sync_entries(
    ws: &Workspace,
    lock: &Lock,
    entries: &[&RepoEntry],
) -> Result<SyncReport, Error> {
    std::fs::create_dir_all(ws.repos_dir()).map_err(Error::io(ws.repos_dir()))?;
    let repos = par_map(entries, |entry| RepoSync {
        repo: entry.name.clone(),
        outcome: sync_one(ws, lock, entry).unwrap_or_else(|e| SyncOutcome::Failed(e.into())),
    });
    Ok(SyncReport { repos })
}

fn sync_one(ws: &Workspace, lock: &Lock, entry: &RepoEntry) -> Result<SyncOutcome, Error> {
    let Some(repo) = ws.repo(entry) else {
        Git::clone_into(
            lock,
            &ws.config.gitlab.ssh_url(&entry.gitlab),
            &ws.repo_dir(&entry.name),
        )?;
        return Ok(SyncOutcome::Cloned);
    };
    repo.git.fetch(lock)?;
    repo.git.ensure_origin_head(lock)?;
    let facts = repo.facts()?;
    if facts.canonical != Canonical::Ok {
        return Ok(SyncOutcome::Skipped {
            canonical: facts.canonical,
        });
    }
    let branch = facts.default_branch;
    if facts.behind == 0 {
        return Ok(SyncOutcome::UpToDate { branch });
    }
    repo.git.merge_ff_only(lock, Rev::Origin(&branch))?;
    Ok(SyncOutcome::FastForwarded {
        branch,
        commits: facts.behind,
    })
}
