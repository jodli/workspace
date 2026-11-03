//! What `drop` and `prune` share: evaluating worktrees against the agent's
//! merge proofs, removing the removable ones, and clearing the space after.

use std::path::PathBuf;
use std::str::FromStr;

use serde::Serialize;

use crate::config::{Forge, MergedQuery};
use crate::error::{Error, InputError};
use crate::git::Upstream;
use crate::names::{BranchName, CommitSha, RemotePath, RepoName, SpaceName};
use crate::space::{self, Found};
use crate::workspace::{Lock, Workspace, par_map};
use crate::worktree::{Checked, Facts, Keep, Removed, Subject, Worktree};

/// `<repo>=<sha>`: the agent saw on the forge that the PR/MR of this repo's
/// branch was merged with this head commit.
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

/// Fetches each repo once, so merged branches show as merged. A failed fetch
/// leaves the old refs, which only makes the checks more conservative.
pub fn refresh<'a>(
    ws: &Workspace,
    lock: &Lock,
    repos: impl Iterator<Item = &'a RepoName>,
) -> Vec<FetchError> {
    let mut repos: Vec<&RepoName> = repos.collect();
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

/// What the agent should look up on the forge before this worktree can go.
#[derive(Debug, Clone, Serialize)]
pub struct MergeCheck {
    pub remote: RemotePath,
    pub source_branch: BranchName,
    /// Run it; an entry whose `head_sha_field` equals the worktree's `head`
    /// proves the work landed.
    #[serde(flatten)]
    pub query: MergedQuery,
    /// Pass this to `drop`/`prune` when it does.
    pub if_merged: String,
}

/// One worktree with the facts its decision was made on.
pub struct Evaluated {
    pub checked: Checked,
    pub facts: Facts,
    /// Present when the branch was pushed and its commits are not on the
    /// default branch: an open PR/MR, or a squash merge.
    pub merge_check: Option<MergeCheck>,
}

fn merge_check(
    forge: &Forge,
    remote: &RemotePath,
    subject: &Subject,
    facts: &Facts,
) -> Option<MergeCheck> {
    let pushed = !matches!(facts.upstream, Upstream::None);
    match (&subject.branch, &facts.head) {
        (Some(branch), Some(head)) if pushed && !facts.on_default() => Some(MergeCheck {
            remote: remote.clone(),
            source_branch: branch.clone(),
            query: forge.merged_query(remote, branch),
            if_merged: format!("--merged {}={head}", subject.repo),
        }),
        _ => None,
    }
}

pub fn evaluate(ws: &Workspace, found: &Found, proofs: &[MergedProof]) -> Result<Evaluated, Error> {
    let repo = ws.cloned(&found.repo)?;
    let default = repo.default_branch()?;
    let wt = Worktree::new(found, repo.git.clone());
    let facts = Facts::read(&wt.git(), &default)?;
    let merged: Vec<&CommitSha> = proofs
        .iter()
        .filter(|p| p.repo == found.repo)
        .map(|p| &p.sha)
        .collect();
    let merge_check = merge_check(&ws.config.forge, &repo.entry.remote, &wt.subject, &facts);
    Ok(Evaluated {
        checked: wt.check(&facts, &merged),
        facts,
        merge_check,
    })
}

/// Evaluates in parallel, keeping order.
pub fn evaluate_all(
    ws: &Workspace,
    found: &[&Found],
    proofs: &[MergedProof],
) -> Result<Vec<Evaluated>, Error> {
    par_map(found, |f| evaluate(ws, f, proofs))
        .into_iter()
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Kept {
    #[serde(flatten)]
    pub subject: Subject,
    #[serde(flatten)]
    pub keep: Keep,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_check: Option<MergeCheck>,
}

#[derive(Debug, Serialize)]
pub struct MissingView {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
}

impl From<Found> for MissingView {
    fn from(f: Found) -> Self {
        Self {
            repo: f.repo,
            path: f.entry.path,
            branch: f.entry.branch,
        }
    }
}

/// What happened to one space.
#[derive(Debug, Serialize)]
pub struct SpaceRemoval {
    pub removed: Vec<Removed>,
    pub kept: Vec<Kept>,
    /// Registry entries of worktrees whose directory was already gone.
    pub pruned: Vec<MissingView>,
    pub archived: Option<PathBuf>,
    pub space_removed: bool,
}

/// Removes what the evaluation allows (everything, when forced), then the
/// space if nothing is left in it.
pub fn remove_space(
    ws: &Workspace,
    lock: &Lock,
    space: &SpaceName,
    evaluated: Vec<Evaluated>,
    missing: Vec<Found>,
    has_strays: bool,
    safety: Safety,
) -> Result<SpaceRemoval, Error> {
    let mut removable = Vec::new();
    let mut kept = Vec::new();
    for e in evaluated {
        let merge_check = e.merge_check;
        match (e.checked, safety) {
            (Checked::Removable(wt), _) => removable.push(wt),
            (Checked::Keep(wt, _), Safety::Forced) => removable.push(wt.force()),
            (Checked::Keep(wt, keep), Safety::Checked) => kept.push(Kept {
                subject: wt.subject,
                keep,
                merge_check,
            }),
        }
    }
    // Each worktree of a space belongs to a different repo, so the removals
    // touch disjoint repos.
    let removed = std::thread::scope(|s| {
        let handles: Vec<_> = removable
            .into_iter()
            .map(|wt| s.spawn(|| wt.remove(lock)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect::<Result<Vec<_>, _>>()
    })?;

    let mut pruned = Vec::new();
    for f in missing {
        ws.cloned(&f.repo)?.git.worktree_prune(lock)?;
        pruned.push(MissingView::from(f));
    }

    let dir = ws.space_dir(space);
    let mut archived = None;
    let mut space_removed = false;
    if kept.is_empty() && !has_strays {
        let notes = dir.join(space::NOTES_FILE);
        if notes.is_file() {
            let target = archive_path(ws, space);
            std::fs::create_dir_all(ws.archive_dir()).map_err(Error::io(ws.archive_dir()))?;
            std::fs::rename(&notes, &target).map_err(Error::io(&notes))?;
            archived = Some(target);
        }
        // remove_dir only succeeds on an empty directory: anything ws did not
        // account for keeps the space alive.
        space_removed = std::fs::remove_dir(&dir).is_ok();
    }
    Ok(SpaceRemoval {
        removed,
        kept,
        pruned,
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
