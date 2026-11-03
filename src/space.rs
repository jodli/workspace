//! Spaces: `spaces/<name>/` with one worktree per repo and a `SPACE.md`.
//!
//! `SPACE.md` starts with TOML frontmatter between `+++` lines, so agents and
//! `ws` can read what the space is for. Facts git already knows (repos,
//! branches) are not stored there; they would go stale.

use std::path::{Path, PathBuf};

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use crate::branch::{ChangeType, IssueType};
use crate::config::parse_toml;
use crate::error::{Error, WorkspaceError};
use crate::git::WorktreeEntry;
use crate::names::{BranchName, JiraKey, RepoName, SpaceName};
use crate::workspace::{Workspace, canonical, par_map};

pub const NOTES_FILE: &str = "SPACE.md";
const FENCE: &str = "+++";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub created: Date,
    #[serde(flatten)]
    pub purpose: Purpose,
}

/// What a space is for. Two runs with the same purpose are the same space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Purpose {
    Spike,
    Ticket {
        ticket: JiraKey,
        issue: IssueType,
        change: ChangeType,
        /// The parent ticket, for sub-tasks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<JiraKey>,
    },
    /// An existing branch pulled into a space, e.g. to review an MR.
    Adopted {
        branch: BranchName,
    },
}

impl Purpose {
    pub fn describe(&self) -> String {
        match self {
            Self::Spike => "a spike".into(),
            Self::Ticket { ticket, change, .. } => format!("ticket {ticket} ({})", change.as_str()),
            Self::Adopted { branch } => format!("adopted branch {branch}"),
        }
    }
}

impl Meta {
    pub fn new(purpose: Purpose) -> Self {
        Self {
            created: jiff::Zoned::now().date(),
            purpose,
        }
    }
}

/// Reads the frontmatter. `Ok(None)` for a missing file or one without
/// frontmatter, which is what spaces from before `ws` 0.1 have.
pub fn read_meta(dir: &Path) -> Result<Option<Meta>, Error> {
    let path = dir.join(NOTES_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(&path)(e)),
    };
    let front = text
        .strip_prefix(FENCE)
        .and_then(|r| r.strip_prefix('\n'))
        .and_then(|r| r.split_once(&format!("\n{FENCE}")))
        .map(|(front, _)| front);
    front.map(|f| parse_toml(&path, f)).transpose()
}

/// Writes `SPACE.md` unless it exists. Returns whether it was written.
pub fn scaffold(dir: &Path, space: &SpaceName, meta: &Meta) -> Result<bool, Error> {
    let path = dir.join(NOTES_FILE);
    if path.exists() {
        return Ok(false);
    }
    let front = toml::to_string(meta).map_err(|e| Error::io(&path)(std::io::Error::other(e)))?;
    let body = match &meta.purpose {
        Purpose::Spike => {
            format!("# {space}\n\n## Question\n\n## Constraints\n\n## Findings\n\n## Verdict\n")
        }
        Purpose::Ticket { ticket, .. } => format!("# {ticket}\n\n## Notes\n"),
        Purpose::Adopted { branch } => format!("# {space}: {branch}\n\n## Notes\n"),
    };
    std::fs::write(&path, format!("{FENCE}\n{front}{FENCE}\n\n{body}"))
        .map_err(Error::io(&path))?;
    Ok(true)
}

/// A worktree found in a space, matched to its owning roster repo.
#[derive(Debug, Clone)]
pub struct Found {
    pub repo: RepoName,
    pub entry: WorktreeEntry,
}

#[derive(Debug)]
pub struct SpaceScan {
    pub name: SpaceName,
    pub dir: PathBuf,
    pub meta: Option<Meta>,
    pub worktrees: Vec<Found>,
    /// Registered with git, directory gone.
    pub missing: Vec<Found>,
    /// Directories that are no roster repo's worktree. Never touched by ws.
    pub strays: Vec<PathBuf>,
}

/// All registered worktrees of all cloned repos, with canonical paths.
fn registered(ws: &Workspace) -> Result<Vec<(PathBuf, Found)>, Error> {
    let repos: Vec<_> = ws.repos().collect();
    let lists = par_map(&repos, |repo| {
        repo.git.worktrees().map(|l| (repo.entry.name.clone(), l))
    });
    let mut out = Vec::new();
    for list in lists {
        let (repo, entries) = list?;
        for entry in entries {
            out.push((
                canonical(&entry.path),
                Found {
                    repo: repo.clone(),
                    entry,
                },
            ));
        }
    }
    Ok(out)
}

fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).map_err(Error::io(dir))? {
        let path = e.map_err(Error::io(dir))?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

pub fn scan_all(ws: &Workspace) -> Result<Vec<SpaceScan>, Error> {
    let dir = ws.spaces_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    // Hidden dirs like `.archive` are not valid space names and drop out here.
    let names: Vec<SpaceName> = subdirs(&dir)?
        .iter()
        .filter_map(|p| p.file_name()?.to_str()?.parse().ok())
        .collect();
    scan(ws, names)
}

pub fn scan_one(ws: &Workspace, name: &SpaceName) -> Result<SpaceScan, Error> {
    if !ws.space_dir(name).is_dir() {
        return Err(WorkspaceError::NoSuchSpace(name.clone()).into());
    }
    scan(ws, vec![name.clone()])?
        .pop()
        .ok_or_else(|| WorkspaceError::NoSuchSpace(name.clone()).into())
}

fn scan(ws: &Workspace, names: Vec<SpaceName>) -> Result<Vec<SpaceScan>, Error> {
    let registered = registered(ws)?;
    let mut out = Vec::new();
    for name in names {
        let dir = ws.space_dir(&name);
        let mut worktrees = Vec::new();
        let mut strays = Vec::new();
        for child in subdirs(&dir)? {
            let key = canonical(&child);
            match registered.iter().find(|(p, _)| *p == key) {
                Some((_, found)) => worktrees.push(found.clone()),
                None => strays.push(child),
            }
        }
        let missing = registered
            .iter()
            .filter(|(path, f)| f.entry.prunable && path.parent() == Some(dir.as_path()))
            .map(|(_, f)| f.clone())
            .collect();
        out.push(SpaceScan {
            meta: read_meta(&dir)?,
            name,
            dir,
            worktrees,
            missing,
            strays,
        });
    }
    Ok(out)
}
