//! Spaces: `spaces/<name>/` with one worktree per repo and a `SPACE.md`.
//!
//! `SPACE.md` starts with TOML frontmatter between `+++` lines, so agents and
//! `ws` can read what the space is for. Facts git already knows (repos,
//! branches) are not stored there; they would go stale.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use crate::branch::{ChangeType, IssueType};
use crate::error::Error;
use crate::git::WorktreeEntry;
use crate::names::{BranchName, JiraKey, RepoName, SpaceName};
use crate::workspace::{Workspace, canonical};

pub const NOTES_FILE: &str = "SPACE.md";
const FENCE: &str = "+++";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Meta {
    Spike {
        created: Date,
    },
    Ticket {
        ticket: JiraKey,
        issue: IssueType,
        change: ChangeType,
        created: Date,
    },
    /// An existing branch pulled into a space, e.g. to review an MR.
    Adopted {
        branch: BranchName,
        created: Date,
    },
}

impl Meta {
    /// Same purpose, ignoring when it was created.
    pub fn same_work(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Spike { .. }, Self::Spike { .. }) => true,
            (
                Self::Ticket {
                    ticket: a,
                    change: ca,
                    ..
                },
                Self::Ticket {
                    ticket: b,
                    change: cb,
                    ..
                },
            ) => a == b && ca == cb,
            (Self::Adopted { branch: a, .. }, Self::Adopted { branch: b, .. }) => a == b,
            _ => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Spike { .. } => "a spike".into(),
            Self::Ticket { ticket, change, .. } => format!("ticket {ticket} ({})", change.as_str()),
            Self::Adopted { branch, .. } => format!("adopted branch {branch}"),
        }
    }
}

pub fn today() -> Date {
    jiff::Zoned::now().date()
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
    let Some(rest) = text.strip_prefix(FENCE).and_then(|r| r.strip_prefix('\n')) else {
        return Ok(None);
    };
    let Some((front, _)) = rest.split_once(&format!("\n{FENCE}")) else {
        return Ok(None);
    };
    toml::from_str(front).map(Some).map_err(|e| {
        crate::error::WorkspaceError::Config {
            path,
            message: e.message().to_owned(),
        }
        .into()
    })
}

/// Writes `SPACE.md` unless it exists. Returns whether it was written.
pub fn scaffold(dir: &Path, space: &SpaceName, meta: &Meta) -> Result<bool, Error> {
    let path = dir.join(NOTES_FILE);
    if path.exists() {
        return Ok(false);
    }
    let front = toml::to_string(meta).map_err(|e| Error::Io {
        path: path.clone(),
        source: std::io::Error::other(e),
    })?;
    let body = match meta {
        Meta::Spike { .. } => {
            format!("# {space}\n\n## Question\n\n## Constraints\n\n## Findings\n\n## Verdict\n")
        }
        Meta::Ticket { ticket, .. } => format!("# {ticket}\n\n## Notes\n"),
        Meta::Adopted { branch, .. } => format!("# {space}: {branch}\n\n## Notes\n"),
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

/// All registered worktrees of all cloned repos, keyed by canonical path.
pub fn registered(ws: &Workspace) -> Result<HashMap<PathBuf, Found>, Error> {
    let mut map = HashMap::new();
    for repo in ws.repos() {
        for entry in repo.git.worktrees()? {
            let key = canonical(&entry.path);
            map.insert(
                key,
                Found {
                    repo: repo.entry.name.clone(),
                    entry,
                },
            );
        }
    }
    Ok(map)
}

/// Scans `spaces/`, or just one space.
pub fn scan(ws: &Workspace, only: Option<&SpaceName>) -> Result<Vec<SpaceScan>, Error> {
    let registered = registered(ws)?;
    let spaces_dir = ws.spaces_dir();
    let names: Vec<SpaceName> = match only {
        Some(n) => {
            if !ws.space_dir(n).is_dir() {
                return Err(crate::error::WorkspaceError::NoSuchSpace(n.clone()).into());
            }
            vec![n.clone()]
        }
        None => match std::fs::read_dir(&spaces_dir) {
            Ok(rd) => {
                let mut v: Vec<SpaceName> = rd
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().to_str()?.parse().ok())
                    .collect();
                v.sort();
                v
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::io(&spaces_dir)(e)),
        },
    };

    let mut out = Vec::new();
    for name in names {
        let dir = ws.space_dir(&name);
        let mut scan = SpaceScan {
            meta: read_meta(&dir)?,
            name,
            worktrees: Vec::new(),
            missing: Vec::new(),
            strays: Vec::new(),
            dir: dir.clone(),
        };
        let mut children: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(Error::io(&dir))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        children.sort();
        for child in children {
            match registered.get(&canonical(&child)) {
                Some(found) => scan.worktrees.push(found.clone()),
                None => scan.strays.push(child),
            }
        }
        scan.missing = registered
            .iter()
            .filter(|(path, f)| f.entry.prunable && path.parent() == Some(dir.as_path()))
            .map(|(_, f)| f.clone())
            .collect();
        out.push(scan);
    }
    Ok(out)
}
