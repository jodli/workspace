//! The git CLI behind typed methods. Arguments go in as vectors, never through
//! a shell, and output is parsed into structs here, so callers never see raw
//! git text.
//!
//! Calling the CLI instead of a library keeps behaviour identical to the git
//! the agent runs itself: ssh config, credential helpers, long paths.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use serde::Serialize;

use crate::error::GitError;
use crate::names::{BranchName, CommitSha};

type Result<T> = std::result::Result<T, GitError>;

/// A repo or worktree directory to run git in.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
}

/// Uncommitted state. Ignored files (build dirs) never count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub tracked: usize,
    pub untracked: usize,
}

impl Changes {
    pub fn is_clean(self) -> bool {
        self.tracked == 0 && self.untracked == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "ref", rename_all = "snake_case")]
pub enum Upstream {
    None,
    /// Configured, but the remote branch was deleted (usually: MR merged).
    Gone,
    Set(String),
}

/// One entry of `git worktree list`.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<CommitSha>,
    pub branch: Option<BranchName>,
    /// The directory is gone but git still has it registered.
    pub prunable: bool,
    pub locked: bool,
}

/// ssh must fail instead of asking for a passphrase or host key confirmation:
/// there is nobody to answer. Only applied when the user has not configured
/// their own ssh command, which would win anyway.
fn ssh_command() -> Option<&'static str> {
    static SSH: OnceLock<bool> = OnceLock::new();
    let user_configured = *SSH.get_or_init(|| {
        std::env::var_os("GIT_SSH_COMMAND").is_some()
            || std::env::var_os("GIT_SSH").is_some()
            || Command::new("git")
                .args(["config", "--global", "--get", "core.sshCommand"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
    });
    (!user_configured).then_some("ssh -o BatchMode=yes -o LogLevel=ERROR")
}

impl Git {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir).args(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        if let Some(ssh) = ssh_command() {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd
    }

    fn output<I, S>(&self, args: I) -> Result<(String, Output)>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
        let shown = args
            .clone()
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let out = self.command(args).output().map_err(GitError::Spawn)?;
        Ok((shown, out))
    }

    /// Runs and returns stdout, failing on any non-zero exit.
    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
        let (shown, out) = self.output(args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
        } else {
            Err(failed(shown, &out))
        }
    }

    /// For commands that answer yes/no with exit 0/1. Anything else is an error.
    fn test<I, S>(&self, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
        let (shown, out) = self.output(args)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(shown, &out)),
        }
    }

    pub fn clone_into(url: &str, dest: &Path) -> Result<()> {
        let parent = dest.parent().unwrap_or(dest);
        Self::at(parent)
            .run([
                OsStr::new("clone"),
                OsStr::new("--quiet"),
                OsStr::new(url),
                dest.as_os_str(),
            ])
            .map(drop)
    }

    pub fn fetch(&self) -> Result<()> {
        self.run(["fetch", "--quiet", "--prune", "origin"])
            .map(drop)
    }

    /// The branch `origin/HEAD` points at. Asks the remote once if the local
    /// pointer is missing, then falls back to the usual names.
    pub fn default_branch(&self) -> Result<BranchName> {
        let read = || -> Result<Option<BranchName>> {
            let (_, out) = self.output([
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ])?;
            let s = String::from_utf8_lossy(&out.stdout);
            Ok(s.trim()
                .strip_prefix("origin/")
                .and_then(|b| b.parse().ok()))
        };
        if let Some(b) = read()? {
            return Ok(b);
        }
        // Network call; failure just means we try the fallbacks.
        let _ = self.output(["remote", "set-head", "origin", "--auto"]);
        if let Some(b) = read()? {
            return Ok(b);
        }
        for candidate in ["main", "master", "trunk", "develop"] {
            if self.ref_exists(&format!("refs/remotes/origin/{candidate}"))? {
                return candidate
                    .parse()
                    .map_err(|_| unexpected("default branch", candidate));
            }
        }
        Err(unexpected(
            "symbolic-ref refs/remotes/origin/HEAD",
            "origin has no default branch",
        ))
    }

    /// `None` when HEAD is detached.
    pub fn head_branch(&self) -> Result<Option<BranchName>> {
        let (shown, out) = self.output(["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        match out.status.code() {
            Some(0) => {
                let s = String::from_utf8_lossy(&out.stdout);
                s.trim()
                    .parse()
                    .map(Some)
                    .map_err(|_| unexpected(&shown, s.trim()))
            }
            Some(1) => Ok(None),
            _ => Err(failed(shown, &out)),
        }
    }

    pub fn head_sha(&self) -> Result<CommitSha> {
        let s = self.run(["rev-parse", "HEAD"])?;
        s.parse().map_err(|_| unexpected("rev-parse HEAD", &s))
    }

    pub fn changes(&self) -> Result<Changes> {
        let s = self.run([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignored=no",
        ])?;
        let mut c = Changes::default();
        let mut entries = s.split('\0').filter(|e| !e.is_empty());
        while let Some(entry) = entries.next() {
            if entry.starts_with("??") {
                c.untracked += 1;
            } else {
                c.tracked += 1;
                // Renames and copies carry the original path as an extra entry.
                if entry.starts_with(['R', 'C']) {
                    entries.next();
                }
            }
        }
        Ok(c)
    }

    pub fn upstream(&self, branch: &BranchName) -> Result<Upstream> {
        let s = self.run([
            "for-each-ref",
            "--format=%(upstream:short)%00%(upstream:track)",
            &format!("refs/heads/{branch}"),
        ])?;
        let (up, track) = s.split_once('\0').unwrap_or((&s, ""));
        Ok(match (up, track) {
            ("", _) => Upstream::None,
            (_, "[gone]") => Upstream::Gone,
            (u, _) => Upstream::Set(u.to_owned()),
        })
    }

    /// Commits in `to` that are not in `from`.
    pub fn count_between(&self, from: &str, to: &str) -> Result<usize> {
        let s = self.run(["rev-list", "--count", &format!("{from}..{to}")])?;
        s.parse().map_err(|_| unexpected("rev-list --count", &s))
    }

    pub fn ref_exists(&self, full_ref: &str) -> Result<bool> {
        self.test(["show-ref", "--verify", "--quiet", full_ref])
    }

    pub fn is_ancestor(&self, commit: &str, of: &str) -> Result<bool> {
        self.test(["merge-base", "--is-ancestor", commit, of])
    }

    /// New branch from `base`, without upstream: otherwise git makes the base
    /// the upstream and `git pull` in the worktree pulls the default branch.
    pub fn worktree_add_new(&self, path: &Path, branch: &BranchName, base: &str) -> Result<()> {
        self.run([
            OsStr::new("-c"),
            OsStr::new("core.longpaths=true"),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            OsStr::new("--no-track"),
            OsStr::new("-b"),
            OsStr::new(branch.as_str()),
            path.as_os_str(),
            OsStr::new(base),
        ])
        .map(drop)
    }

    pub fn worktree_add_existing(&self, path: &Path, branch: &BranchName) -> Result<()> {
        self.run([
            OsStr::new("-c"),
            OsStr::new("core.longpaths=true"),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
            path.as_os_str(),
            OsStr::new(branch.as_str()),
        ])
        .map(drop)
    }

    /// Only reachable through `worktree::Worktree<Removable>`.
    pub(crate) fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(path.as_os_str());
        self.run(args).map(drop)
    }

    pub fn worktree_move(&self, from: &Path, to: &Path) -> Result<()> {
        self.run([
            OsStr::new("worktree"),
            OsStr::new("move"),
            from.as_os_str(),
            to.as_os_str(),
        ])
        .map(drop)
    }

    pub fn worktree_prune(&self) -> Result<()> {
        self.run(["worktree", "prune"]).map(drop)
    }

    pub fn worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let s = self.run(["worktree", "list", "--porcelain", "-z"])?;
        let mut out = Vec::new();
        let mut cur: Option<WorktreeEntry> = None;
        for field in s.split('\0') {
            if field.is_empty() {
                out.extend(cur.take());
                continue;
            }
            let (key, value) = field.split_once(' ').unwrap_or((field, ""));
            match (key, cur.as_mut()) {
                ("worktree", _) => {
                    out.extend(cur.take());
                    cur = Some(WorktreeEntry {
                        path: PathBuf::from(value),
                        head: None,
                        branch: None,
                        prunable: false,
                        locked: false,
                    });
                }
                ("HEAD", Some(w)) => w.head = value.parse().ok(),
                ("branch", Some(w)) => {
                    w.branch = value
                        .strip_prefix("refs/heads/")
                        .and_then(|b| b.parse().ok());
                }
                ("prunable", Some(w)) => w.prunable = true,
                ("locked", Some(w)) => w.locked = true,
                _ => {}
            }
        }
        out.extend(cur);
        Ok(out)
    }

    pub fn branch_delete(&self, branch: &BranchName, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        self.run(["branch", "--quiet", flag, branch.as_str()])
            .map(drop)
    }

    pub fn branch_track(&self, branch: &BranchName) -> Result<()> {
        self.run([
            "branch",
            "--quiet",
            "--track",
            branch.as_str(),
            &format!("origin/{branch}"),
        ])
        .map(drop)
    }

    pub fn checkout(&self, branch: &BranchName) -> Result<()> {
        self.run(["checkout", "--quiet", branch.as_str()]).map(drop)
    }

    pub fn merge_ff_only(&self, target: &str) -> Result<()> {
        self.run(["merge", "--ff-only", "--quiet", target])
            .map(drop)
    }

    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        let (shown, out) = self.output(["config", "--get", key])?;
        match out.status.code() {
            Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
            Some(1) => Ok(None),
            _ => Err(failed(shown, &out)),
        }
    }

    pub fn version() -> Result<String> {
        Self::at(".").run(["--version"])
    }
}

fn failed(args: String, out: &Output) -> GitError {
    GitError::Failed {
        args,
        code: out
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| format!("exit {c}")),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    }
}

fn unexpected(args: &str, output: &str) -> GitError {
    GitError::Unexpected {
        args: args.to_owned(),
        output: output.to_owned(),
    }
}
