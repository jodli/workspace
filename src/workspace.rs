//! The workspace root and everything addressed relative to it.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{Error, WorkspaceError};
use crate::git::Git;
use crate::names::{RepoName, SpaceName};
use crate::roster::{RepoEntry, Roster};

pub const CONFIG_FILE: &str = "ws.toml";
const ROSTER_FILE: &str = "repos.tsv";
const LOCK_FILE: &str = ".ws.lock";
const LOCK_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub config: Config,
    pub roster: Roster,
}

/// A roster repo that is cloned.
#[derive(Debug, Clone)]
pub struct Repo<'w> {
    pub entry: &'w RepoEntry,
    pub dir: PathBuf,
    pub git: Git,
}

/// Held while a command mutates the workspace. Released on drop.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] File);

impl Workspace {
    /// `WS_ROOT` if set, else the nearest parent of the current directory
    /// that has a `ws.toml`.
    pub fn discover() -> Result<Self, Error> {
        let root = if let Some(r) = std::env::var_os("WS_ROOT") {
            PathBuf::from(r)
        } else {
            let cwd = std::env::current_dir().map_err(Error::io("."))?;
            find_root(&cwd).ok_or(WorkspaceError::NotFound(cwd))?
        };
        Self::open(&root)
    }

    pub fn open(root: &Path) -> Result<Self, Error> {
        let config_path = root.join(CONFIG_FILE);
        if !config_path.is_file() {
            return Err(WorkspaceError::NotFound(root.to_owned()).into());
        }
        // Canonical once, so every path we hand out or compare with git output
        // has the same form. dunce keeps Windows paths free of `\\?\`, which
        // git does not accept.
        let root = dunce::canonicalize(root).map_err(Error::io(root))?;
        Ok(Self {
            config: Config::load(&config_path)?,
            roster: Roster::load(&root.join(ROSTER_FILE))?,
            root,
        })
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    pub fn spaces_dir(&self) -> PathBuf {
        self.root.join("spaces")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.spaces_dir().join(".archive")
    }

    pub fn repo_dir(&self, repo: &RepoName) -> PathBuf {
        self.repos_dir().join(repo)
    }

    pub fn space_dir(&self, space: &SpaceName) -> PathBuf {
        self.spaces_dir().join(space)
    }

    /// The repo if it is cloned, `None` if it is only in the roster.
    pub fn repo<'w>(&'w self, entry: &'w RepoEntry) -> Option<Repo<'w>> {
        let dir = self.repo_dir(&entry.name);
        dir.join(".git").exists().then(|| Repo {
            entry,
            git: Git::at(&dir),
            dir,
        })
    }

    /// Like `repo`, for callers that need it cloned.
    pub fn cloned<'w>(&'w self, name: &RepoName) -> Result<Repo<'w>, Error> {
        let entry = self.roster.require(name)?;
        self.repo(entry).ok_or_else(|| {
            WorkspaceError::RepoMissing {
                repo: name.clone(),
                path: self.repo_dir(name),
            }
            .into()
        })
    }

    pub fn repos(&self) -> impl Iterator<Item = Repo<'_>> {
        self.roster.iter().filter_map(|e| self.repo(e))
    }

    /// Serializes mutating commands across parallel agents.
    pub fn lock(&self) -> Result<Lock, Error> {
        let path = self.root.join(LOCK_FILE);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(Error::io(&path))?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock(file)),
                Err(TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(TryLockError::WouldBlock) => return Err(Error::Busy(path)),
                Err(TryLockError::Error(e)) => return Err(Error::io(&path)(e)),
            }
        }
    }
}

/// The nearest directory at or above `from` that has a `ws.toml`.
pub fn find_root(from: &Path) -> Option<PathBuf> {
    from.ancestors()
        .find(|d| d.join(CONFIG_FILE).is_file())
        .map(Path::to_path_buf)
}

/// Canonical form for comparing paths, falling back to the path itself when it
/// does not exist (git keeps worktrees registered after their dir is gone).
pub fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}
