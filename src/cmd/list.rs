//! `ws list` and `ws status <space>`: the state an agent needs before acting,
//! in one call.

use std::path::PathBuf;

use serde::Serialize;

use super::{Defaults, Report};
use crate::branch::PrefixStyle;
use crate::error::Error;
use crate::git::{Changes, Upstream};
use crate::names::{BranchName, CommitSha, GitlabPath, RepoName, SpaceName};
use crate::space::{self, Found, Meta, SpaceScan};
use crate::workspace::Workspace;

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
    pub gitlab: GitlabPath,
    pub style: PrefixStyle,
    pub path: PathBuf,
    pub cloned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<RepoState>,
}

#[derive(Debug, Serialize)]
pub struct RepoState {
    pub default_branch: BranchName,
    /// `None` when detached.
    pub branch: Option<BranchName>,
    /// `repos/` must sit on the default branch; false is a problem to fix.
    pub on_default: bool,
    pub changes: Changes,
    pub behind: usize,
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
    pub head: Option<CommitSha>,
    pub changes: Changes,
    pub upstream: Upstream,
    /// Commits not on the upstream, or not on the default branch when there
    /// is no live upstream.
    pub unpushed: usize,
    pub default_branch: BranchName,
    pub ahead_of_default: usize,
}

#[derive(Debug, Serialize)]
pub struct MissingView {
    pub repo: RepoName,
    pub path: PathBuf,
    pub branch: Option<BranchName>,
}

pub fn list(ws: &Workspace) -> Result<ListReport, Error> {
    let mut defaults = Defaults::default();
    let mut repos = Vec::new();
    for entry in ws.roster.iter() {
        let state = match ws.repo(entry) {
            None => None,
            Some(repo) => {
                let default_branch = defaults.of(&repo)?;
                let branch = repo.git.head_branch()?;
                Some(RepoState {
                    on_default: branch.as_ref() == Some(&default_branch),
                    behind: repo
                        .git
                        .count_between("HEAD", &format!("origin/{default_branch}"))?,
                    changes: repo.git.changes()?,
                    branch,
                    default_branch,
                })
            }
        };
        repos.push(RepoView {
            name: entry.name.clone(),
            gitlab: entry.gitlab.clone(),
            style: entry.style,
            path: ws.repo_dir(&entry.name),
            cloned: state.is_some(),
            state,
        });
    }
    let spaces = space::scan(ws, None)?
        .into_iter()
        .map(|s| space_view(ws, &mut defaults, s))
        .collect::<Result<_, _>>()?;
    Ok(ListReport {
        root: ws.root.clone(),
        repos,
        spaces,
    })
}

pub fn status(ws: &Workspace, name: &SpaceName) -> Result<SpaceView, Error> {
    let mut defaults = Defaults::default();
    let scan = space::scan(ws, Some(name))?;
    let s = scan
        .into_iter()
        .next()
        .ok_or_else(|| crate::error::WorkspaceError::NoSuchSpace(name.clone()))?;
    space_view(ws, &mut defaults, s)
}

pub fn space_view(
    ws: &Workspace,
    defaults: &mut Defaults,
    s: SpaceScan,
) -> Result<SpaceView, Error> {
    let notes = s.dir.join(space::NOTES_FILE);
    Ok(SpaceView {
        worktrees: s
            .worktrees
            .iter()
            .map(|f| worktree_view(ws, defaults, f))
            .collect::<Result<_, _>>()?,
        missing: s
            .missing
            .into_iter()
            .map(|f| MissingView {
                repo: f.repo,
                path: f.entry.path,
                branch: f.entry.branch,
            })
            .collect(),
        notes: notes.is_file().then_some(notes),
        name: s.name,
        path: s.dir,
        meta: s.meta,
        strays: s.strays,
    })
}

pub fn worktree_view(
    ws: &Workspace,
    defaults: &mut Defaults,
    f: &Found,
) -> Result<WorktreeView, Error> {
    let repo = ws.cloned(&f.repo)?;
    let default_branch = defaults.of(&repo)?;
    let git = crate::git::Git::at(&f.entry.path);
    let upstream = match &f.entry.branch {
        Some(b) => git.upstream(b)?,
        None => Upstream::None,
    };
    let base = format!("origin/{default_branch}");
    let ahead_of_default = git.count_between(&base, "HEAD")?;
    let unpushed = match &upstream {
        Upstream::Set(u) => git.count_between(u, "HEAD")?,
        Upstream::None | Upstream::Gone => ahead_of_default,
    };
    Ok(WorktreeView {
        repo: f.repo.clone(),
        path: f.entry.path.clone(),
        branch: f.entry.branch.clone(),
        head: f.entry.head.clone(),
        changes: git.changes()?,
        upstream,
        unpushed,
        default_branch,
        ahead_of_default,
    })
}
