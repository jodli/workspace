//! `ws list` and `ws status <space>`: the state an agent needs before acting,
//! in one call.

use std::path::PathBuf;

use serde::Serialize;

use super::Report;
use crate::branch::PrefixStyle;
use crate::error::Error;
use crate::git::Git;
use crate::names::{BranchName, RemotePath, RepoName, SpaceName};
use crate::removal::MissingView;
use crate::space::{self, Found, Meta, SpaceScan};
use crate::workspace::{RepoFacts, Workspace, par_map};
use crate::worktree::Facts;

#[derive(Debug, Serialize)]
pub struct ListReport {
    pub root: PathBuf,
    pub repos: Vec<RepoView>,
    pub spaces: Vec<SpaceView>,
}

impl Report for ListReport {}

#[derive(Debug, Serialize)]
pub struct RepoView {
    pub name: RepoName,
    pub remote: RemotePath,
    pub style: PrefixStyle,
    pub path: PathBuf,
    /// `null` when the repo is in the roster but not cloned.
    pub state: Option<RepoFacts>,
}

#[derive(Debug, Serialize)]
pub struct SpaceView {
    pub name: SpaceName,
    pub path: PathBuf,
    pub meta: Option<Meta>,
    pub notes: Option<PathBuf>,
    pub worktrees: Vec<WorktreeView>,
    /// Registered with git but the directory is gone.
    pub missing: Vec<MissingView>,
    /// Directories that are not worktrees of any roster repo.
    pub strays: Vec<PathBuf>,
}

impl Report for SpaceView {}

#[derive(Debug, Serialize)]
pub struct WorktreeView {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
    pub default_branch: BranchName,
    #[serde(flatten)]
    pub facts: Facts,
}

pub fn list(ws: &Workspace) -> Result<ListReport, Error> {
    let entries: Vec<_> = ws.roster.iter().collect();
    let repos = par_map(&entries, |entry| {
        Ok::<_, Error>(RepoView {
            name: entry.name.clone(),
            remote: entry.remote.clone(),
            style: entry.style,
            path: ws.repo_dir(&entry.name),
            state: ws.repo(entry).map(|r| r.facts()).transpose()?,
        })
    })
    .into_iter()
    .collect::<Result<_, _>>()?;
    Ok(ListReport {
        root: ws.root.clone(),
        repos,
        spaces: space_views(ws, space::scan_all(ws)?)?,
    })
}

pub fn status(ws: &Workspace, name: &SpaceName) -> Result<SpaceView, Error> {
    let mut views = space_views(ws, vec![space::scan_one(ws, name)?])?;
    views
        .pop()
        .ok_or_else(|| crate::error::WorkspaceError::NoSuchSpace(name.clone()).into())
}

/// Reads all worktrees of all given spaces in parallel, then regroups.
pub fn space_views(ws: &Workspace, scans: Vec<SpaceScan>) -> Result<Vec<SpaceView>, Error> {
    let all: Vec<&Found> = scans.iter().flat_map(|s| &s.worktrees).collect();
    let mut views = par_map(&all, |f| worktree_view(ws, f)).into_iter();
    let mut out = Vec::with_capacity(scans.len());
    for s in scans {
        let worktrees = views
            .by_ref()
            .take(s.worktrees.len())
            .collect::<Result<_, _>>()?;
        let notes = s.dir.join(space::NOTES_FILE);
        out.push(SpaceView {
            missing: s.missing.into_iter().map(MissingView::from).collect(),
            notes: notes.is_file().then_some(notes),
            name: s.name,
            path: s.dir,
            meta: s.meta,
            strays: s.strays,
            worktrees,
        });
    }
    Ok(out)
}

pub fn worktree_view(ws: &Workspace, f: &Found) -> Result<WorktreeView, Error> {
    let default_branch = ws.cloned(&f.repo)?.default_branch()?;
    Ok(WorktreeView {
        facts: Facts::read(&Git::at(&f.entry.path), &default_branch)?,
        repo: f.repo.clone(),
        path: f.entry.path.clone(),
        branch: f.entry.branch.clone(),
        default_branch,
    })
}
