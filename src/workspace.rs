//! The workspace root and everything addressed relative to it.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, WorkspaceError};
use crate::git::{Git, Rev, Untracked, WorktreeEntry};
use crate::names::{BranchName, RepoName, SpaceName};
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
    defaults: Mutex<HashMap<RepoName, BranchName>>,
}

/// Proof that the workspace lock is held. Only `Workspace::lock` makes one,
/// and every mutating git call asks for it.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] File);

/// A roster repo that is cloned.
#[derive(Debug, Clone)]
pub struct Repo<'w> {
    pub entry: &'w RepoEntry,
    pub git: Git,
    ws: &'w Workspace,
}

/// Whether `repos/<repo>` is the clean default-branch checkout it must be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Canonical {
    Ok,
    NotOnDefault { branch: Option<BranchName> },
    Uncommitted { tracked: usize },
    Ahead { commits: usize },
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoFacts {
    pub default_branch: BranchName,
    pub branch: Option<BranchName>,
    pub canonical: Canonical,
    pub behind: usize,
}

/// What occupies `spaces/<space>/<repo>`.
#[derive(Debug, Clone)]
pub enum Slot {
    Free,
    /// A worktree of this repo.
    Ours(WorktreeEntry),
    /// Something else. ws never touches it.
    Occupied,
}

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
            defaults: Mutex::default(),
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
            git: Git::at(dir),
            ws: self,
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

impl Repo<'_> {
    pub fn dir(&self) -> PathBuf {
        self.ws.repo_dir(&self.entry.name)
    }

    /// Asked once per repo and run.
    pub fn default_branch(&self) -> Result<BranchName, Error> {
        let cached = self
            .ws
            .defaults
            .lock()
            .ok()
            .and_then(|m| m.get(&self.entry.name).cloned());
        if let Some(b) = cached {
            return Ok(b);
        }
        let b = self.git.default_branch()?;
        if let Ok(mut m) = self.ws.defaults.lock() {
            m.insert(self.entry.name.clone(), b.clone());
        }
        Ok(b)
    }

    pub fn facts(&self) -> Result<RepoFacts, Error> {
        let default_branch = self.default_branch()?;
        let status = self.git.status(Untracked::Skip)?;
        let (ahead, behind) = self.git.ahead_behind(Rev::Origin(&default_branch))?;
        // Untracked files do not count: they block nothing and sync's
        // fast-forward refuses by itself if one would be overwritten.
        let canonical = if status.branch.as_ref() != Some(&default_branch) {
            Canonical::NotOnDefault {
                branch: status.branch.clone(),
            }
        } else if status.changes.tracked > 0 {
            Canonical::Uncommitted {
                tracked: status.changes.tracked,
            }
        } else if ahead > 0 {
            Canonical::Ahead { commits: ahead }
        } else {
            Canonical::Ok
        };
        Ok(RepoFacts {
            branch: status.branch,
            default_branch,
            canonical,
            behind,
        })
    }

    /// What is at `path`, from this repo's point of view.
    pub fn slot(&self, path: &Path) -> Result<Slot, Error> {
        if !path.exists() {
            return Ok(Slot::Free);
        }
        let want = canonical(path);
        Ok(self
            .git
            .worktrees()?
            .into_iter()
            .find(|w| canonical(&w.path) == want)
            .map_or(Slot::Occupied, Slot::Ours))
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

/// Runs `f` over `items` on up to 8 threads, keeping order. Threads take
/// the next unclaimed item, so a few slow items do not stall one thread with
/// a fixed share. For independent per-repo, per-worktree and per-file work.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let next = AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads.min(items.len()))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break done };
                        done.push((i, f(item)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            // A panicking closure is a bug: surface it instead of returning
            // fewer results than items.
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    out.sort_unstable_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}
