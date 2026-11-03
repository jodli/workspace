//! `ws drop <space>`: remove a space's worktrees, but only those proven safe.
//! The space itself goes once nothing is left in it; `SPACE.md` is archived.

use std::path::PathBuf;
use std::str::FromStr;

use serde::Serialize;

use super::Report;
use super::list::MissingView;
use crate::error::{Error, Exit, InputError};
use crate::names::{BranchName, CommitSha, RepoName, SpaceName};
use crate::space::{self, Found, SpaceScan};
use crate::workspace::{Lock, Workspace, par_map};
use crate::worktree::{Checked, Facts, Keep, Removed, Worktree};

/// `<repo>=<sha>`: the agent saw in GitLab that the MR of this repo's branch
/// was merged with this head commit.
#[derive(Debug, Clone)]
pub struct MergedProof {
    pub repo: RepoName,
    pub sha: CommitSha,
}

impl FromStr for MergedProof {
    type Err = InputError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || InputError::MergedProof(s.to_owned());
        let (repo, sha) = s.split_once('=').ok_or_else(bad)?;
        Ok(Self {
            repo: repo.parse().map_err(|_| bad())?,
            sha: sha.parse().map_err(|_| bad())?,
        })
    }
}

/// Removal is checked unless the caller explicitly gives that up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    Checked,
    Forced,
}

#[derive(Debug, Serialize)]
pub struct FetchError {
    pub repo: RepoName,
    pub error: String,
}

/// Fetches the repos once each, so merged branches show as merged. A failed
/// fetch leaves the old refs, which only makes the checks more conservative.
pub(crate) fn refresh(ws: &Workspace, lock: &Lock, scans: &[&SpaceScan]) -> Vec<FetchError> {
    let mut repos: Vec<&RepoName> = scans
        .iter()
        .flat_map(|s| s.worktrees.iter().map(|f| &f.repo))
        .collect();
    repos.sort();
    repos.dedup();
    par_map(&repos, |repo| {
        let r = ws.cloned(repo).and_then(|r| Ok(r.git.fetch(lock)?));
        r.err().map(|e| FetchError {
            repo: (*repo).clone(),
            error: e.to_string(),
        })
    })
    .into_iter()
    .flatten()
    .collect()
}

#[derive(Debug, Serialize)]
pub struct DropReport {
    pub space: SpaceName,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fetch_errors: Vec<FetchError>,
    pub removed: Vec<Removed>,
    pub kept: Vec<Kept>,
    /// Registry entries of worktrees whose directory was already gone.
    pub pruned: Vec<MissingView>,
    pub strays: Vec<PathBuf>,
    pub archived: Option<PathBuf>,
    pub space_removed: bool,
}

#[derive(Debug, Serialize)]
pub struct Kept {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
    pub head: Option<CommitSha>,
    #[serde(flatten)]
    pub keep: Keep,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Report for DropReport {
    fn exit(&self) -> Exit {
        Exit::incomplete_if(!self.space_removed)
    }
}

pub fn drop(
    ws: &Workspace,
    name: &SpaceName,
    proofs: &[MergedProof],
    safety: Safety,
) -> Result<DropReport, Error> {
    let lock = ws.lock()?;
    let scan = space::scan_one(ws, name)?;
    if let Some(p) = proofs
        .iter()
        .find(|p| !scan.worktrees.iter().any(|f| f.repo == p.repo))
    {
        return Err(InputError::MergedUnused(p.repo.clone()).into());
    }
    let fetch_errors = refresh(ws, &lock, &[&scan]);
    let mut report = remove_space(ws, &lock, scan, proofs, safety)?;
    report.fetch_errors = fetch_errors;
    Ok(report)
}

/// The decision for one worktree, with the facts it was made on.
pub(crate) fn evaluate(
    ws: &Workspace,
    found: &Found,
    proofs: &[MergedProof],
) -> Result<(Checked, Facts, BranchName), Error> {
    let repo = ws.cloned(&found.repo)?;
    let default = repo.default_branch()?;
    let wt = Worktree::new(found, repo.git.clone());
    let facts = Facts::read(&wt.git(), &default)?;
    // A proof for this repo that names another commit is reported as
    // HeadMoved, so the agent learns its sha was stale.
    let proof = proofs
        .iter()
        .filter(|p| p.repo == found.repo)
        .find(|p| facts.head.as_ref() == Some(&p.sha))
        .or_else(|| proofs.iter().find(|p| p.repo == found.repo))
        .map(|p| &p.sha);
    Ok((wt.check(&facts, proof), facts, default))
}

pub(crate) fn kept(
    wt: &Worktree<crate::worktree::Unchecked>,
    facts: &Facts,
    keep: Keep,
    gitlab: &str,
) -> Kept {
    // An unpushed count with a deleted upstream is the squash-merge pattern:
    // the commits are on GitLab's side under another sha.
    let hint = match (&keep, &facts.upstream, &wt.branch, &facts.head) {
        (Keep::Unpushed { .. }, crate::git::Upstream::Gone { .. }, Some(b), Some(head)) => {
            Some(format!(
                "upstream is gone; if the MR for {b} in {gitlab} was merged at {head}, pass --merged {}={head}",
                wt.repo
            ))
        }
        _ => None,
    };
    Kept {
        repo: wt.repo.clone(),
        path: wt.path.clone(),
        branch: wt.branch.clone(),
        head: facts.head.clone(),
        keep,
        hint,
    }
}

pub(crate) fn remove_space(
    ws: &Workspace,
    lock: &Lock,
    scan: SpaceScan,
    proofs: &[MergedProof],
    safety: Safety,
) -> Result<DropReport, Error> {
    let mut removed = Vec::new();
    let mut kept_list = Vec::new();
    for found in &scan.worktrees {
        let (checked, facts, default) = evaluate(ws, found, proofs)?;
        let removable = match (checked, safety) {
            (Checked::Removable(wt), _) => wt,
            (Checked::Keep(wt, _), Safety::Forced) => wt.force(),
            (Checked::Keep(wt, keep), Safety::Checked) => {
                let gitlab = ws.roster.require(&found.repo)?.gitlab.to_string();
                kept_list.push(kept(&wt, &facts, keep, &gitlab));
                continue;
            }
        };
        removed.push(removable.remove(lock, &default)?);
    }

    let mut pruned = Vec::new();
    for f in scan.missing {
        ws.cloned(&f.repo)?.git.worktree_prune(lock)?;
        pruned.push(MissingView {
            repo: f.repo,
            path: f.entry.path,
            branch: f.entry.branch,
        });
    }

    let mut archived = None;
    let mut space_removed = false;
    if kept_list.is_empty() && scan.strays.is_empty() {
        let notes = scan.dir.join(space::NOTES_FILE);
        if notes.is_file() {
            let target = archive_path(ws, &scan.name);
            std::fs::create_dir_all(ws.archive_dir()).map_err(Error::io(ws.archive_dir()))?;
            std::fs::rename(&notes, &target).map_err(Error::io(&notes))?;
            archived = Some(target);
        }
        // remove_dir only succeeds on an empty directory: anything ws did not
        // account for keeps the space alive.
        space_removed = std::fs::remove_dir(&scan.dir).is_ok();
    }

    Ok(DropReport {
        space: scan.name,
        fetch_errors: Vec::new(),
        removed,
        kept: kept_list,
        pruned,
        strays: scan.strays,
        archived,
        space_removed,
    })
}

/// `.archive/<space>.md`, or with a timestamp suffix if a space of the same
/// name was archived before. Never an existing file: rename would overwrite it.
fn archive_path(ws: &Workspace, name: &SpaceName) -> PathBuf {
    let dir = ws.archive_dir();
    let plain = dir.join(format!("{name}.md"));
    if plain.exists() {
        dir.join(format!("{name}-{}.md", jiff::Timestamp::now().as_second()))
    } else {
        plain
    }
}
