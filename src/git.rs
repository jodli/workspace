//! The git CLI behind typed methods. Arguments go in as vectors, never through
//! a shell, refs are built from `Rev`, and output is parsed into structs here,
//! so callers never handle raw git text.
//!
//! Calling the CLI instead of a library keeps behaviour identical to the git
//! the agent runs itself: ssh config, credential helpers, long paths.
//!
//! Every method that changes a repo takes `&Lock`, so it cannot be called
//! without holding the workspace lock.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use serde::Serialize;

use crate::error::GitError;
use crate::names::{BranchName, CommitSha};
use crate::workspace::Lock;

type Result<T> = std::result::Result<T, GitError>;

/// A revision, rendered as an unambiguous full ref.
#[derive(Debug, Clone, Copy)]
pub enum Rev<'a> {
    Head,
    Local(&'a BranchName),
    /// `origin/<branch>`, the only remote ws knows.
    Origin(&'a BranchName),
}

impl Rev<'_> {
    fn spec(self) -> String {
        match self {
            Self::Head => "HEAD".to_owned(),
            Self::Local(b) => format!("refs/heads/{b}"),
            Self::Origin(b) => format!("refs/remotes/origin/{b}"),
        }
    }
}

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
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Upstream {
    None,
    /// Configured, but the remote branch was deleted (usually: MR merged).
    Gone {
        name: String,
    },
    Set {
        name: String,
        ahead: usize,
        behind: usize,
    },
}

/// One `git status --porcelain=v2 --branch` call.
#[derive(Debug, Clone)]
pub struct Status {
    /// `None` when detached.
    pub branch: Option<BranchName>,
    /// `None` before the first commit.
    pub head: Option<CommitSha>,
    pub upstream: Upstream,
    pub changes: Changes,
}

/// Whether `status` looks for untracked files. Skipping them is much faster
/// in big repos and enough where only tracked changes matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untracked {
    Count,
    Skip,
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

/// Does a command talk to the remote? Only those get the batch-mode ssh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Net {
    Yes,
    No,
}

/// ssh must fail instead of asking for a passphrase or host key confirmation:
/// there is nobody to answer. Only applied when the user has not configured
/// their own ssh command, which would win anyway.
fn batch_ssh() -> Option<&'static str> {
    static USER_SSH: OnceLock<bool> = OnceLock::new();
    let user_configured = *USER_SSH.get_or_init(|| {
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

    fn command<I, S>(&self, net: Net, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        // Worktrees nest one level deeper than clones; node_modules paths
        // cross 260 characters on Windows otherwise. Ignored elsewhere.
        cmd.args(["-c", "core.longpaths=true", "-C"])
            .arg(&self.dir)
            .args(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        if net == Net::Yes
            && let Some(ssh) = batch_ssh()
        {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd
    }

    fn output<I, S>(&self, net: Net, args: I) -> Result<(Command, Output)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command(net, args);
        let out = cmd.output().map_err(GitError::Spawn)?;
        Ok((cmd, out))
    }

    /// Runs and returns stdout, failing on any non-zero exit.
    fn read<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.read_net(Net::No, args)
    }

    fn read_net<I, S>(&self, net: Net, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (cmd, out) = self.output(net, args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
        } else {
            Err(failed(&cmd, &out))
        }
    }

    fn exec<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.read(args).map(drop)
    }

    /// For commands that answer with exit 0 (stdout) or 1 (no). Anything else
    /// is an error.
    fn query<I, S>(&self, args: I) -> Result<Option<String>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (cmd, out) = self.output(Net::No, args)?;
        match out.status.code() {
            Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
            Some(1) => Ok(None),
            _ => Err(failed(&cmd, &out)),
        }
    }

    pub fn version() -> Result<String> {
        Self::at(".").read(["--version"])
    }

    pub fn clone_into(_: &Lock, url: &str, dest: &Path) -> Result<()> {
        let parent = dest.parent().unwrap_or(dest);
        Self::at(parent)
            .read_net(
                Net::Yes,
                [
                    OsStr::new("clone"),
                    OsStr::new("--quiet"),
                    OsStr::new(url),
                    dest.as_os_str(),
                ],
            )
            .map(drop)
    }

    /// Reads the remote's branch list; proves the ssh path git uses works.
    pub fn ls_remote(url: &str) -> Result<()> {
        Self::at(".")
            .read_net(Net::Yes, ["ls-remote", "--heads", url])
            .map(drop)
    }

    pub fn fetch(&self, _: &Lock) -> Result<()> {
        self.read_net(Net::Yes, ["fetch", "--quiet", "--prune", "origin"])
            .map(drop)
    }

    /// Sets `origin/HEAD` from the remote if it is missing. Network.
    pub fn ensure_origin_head(&self, _: &Lock) -> Result<()> {
        if self
            .query(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?
            .is_none()
        {
            self.read_net(Net::Yes, ["remote", "set-head", "origin", "--auto"])?;
        }
        Ok(())
    }

    /// The branch `origin/HEAD` points at, else the first of the usual names
    /// that exists on origin. Local only; `sync` keeps `origin/HEAD` current.
    pub fn default_branch(&self) -> Result<BranchName> {
        const USUAL: [&str; 4] = ["main", "master", "trunk", "develop"];
        let pointed = self.query([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])?;
        if let Some(b) = pointed.as_deref().and_then(|s| s.strip_prefix("origin/")) {
            return b
                .parse()
                .map_err(|_| unexpected("symbolic-ref origin/HEAD", b));
        }
        let present = self.read(
            std::iter::once("for-each-ref".to_owned())
                .chain(std::iter::once("--format=%(refname:lstrip=3)".to_owned()))
                .chain(USUAL.iter().map(|b| format!("refs/remotes/origin/{b}"))),
        )?;
        let present: Vec<&str> = present.lines().collect();
        USUAL
            .iter()
            .find(|b| present.contains(b))
            .and_then(|b| b.parse().ok())
            .ok_or_else(|| {
                unexpected(
                    "symbolic-ref refs/remotes/origin/HEAD",
                    "origin has no default branch",
                )
            })
    }

    pub fn status(&self, untracked: Untracked) -> Result<Status> {
        let mode = match untracked {
            Untracked::Count => "--untracked-files=normal",
            Untracked::Skip => "--untracked-files=no",
        };
        let s = self.read([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--ignored=no",
            mode,
        ])?;
        parse_status(&s).ok_or_else(|| unexpected("status --porcelain=v2", &s))
    }

    /// `(ahead, behind)` of HEAD relative to `base`.
    pub fn ahead_behind(&self, base: Rev<'_>) -> Result<(usize, usize)> {
        let range = format!("{}...HEAD", base.spec());
        let s = self.read(["rev-list", "--left-right", "--count", &range])?;
        let mut nums = s.split_whitespace().map(str::parse::<usize>);
        match (nums.next(), nums.next()) {
            (Some(Ok(behind)), Some(Ok(ahead))) => Ok((ahead, behind)),
            _ => Err(unexpected("rev-list --left-right --count", &s)),
        }
    }

    pub fn has(&self, rev: Rev<'_>) -> Result<bool> {
        Ok(self
            .query(["show-ref", "--verify", "--quiet", &rev.spec()])?
            .is_some())
    }

    /// Adds a worktree. With `new_from`, creates the branch there without an
    /// upstream: otherwise git makes the base the upstream and `git pull` in
    /// the worktree pulls the default branch.
    pub fn worktree_add(
        &self,
        _: &Lock,
        path: &Path,
        branch: &BranchName,
        new_from: Option<Rev<'_>>,
    ) -> Result<()> {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--quiet"),
        ];
        let base = new_from.map(Rev::spec);
        match &base {
            Some(base) => {
                args.extend([
                    OsStr::new("--no-track"),
                    OsStr::new("-b"),
                    OsStr::new(branch.as_str()),
                ]);
                args.extend([path.as_os_str(), OsStr::new(base)]);
            }
            None => args.extend([path.as_os_str(), OsStr::new(branch.as_str())]),
        }
        self.exec(args)
    }

    /// Only reachable through `worktree::Worktree<Removable>`.
    pub(crate) fn worktree_remove(&self, _: &Lock, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(path.as_os_str());
        self.exec(args)
    }

    pub fn worktree_prune(&self, _: &Lock) -> Result<()> {
        self.exec(["worktree", "prune"])
    }

    pub fn worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let s = self.read(["worktree", "list", "--porcelain", "-z"])?;
        Ok(parse_worktrees(&s))
    }

    pub fn branch_delete(&self, _: &Lock, branch: &BranchName) -> Result<()> {
        self.exec(["branch", "--quiet", "-D", branch.as_str()])
    }

    /// Local branch tracking `origin/<branch>`.
    pub fn branch_track(&self, _: &Lock, branch: &BranchName) -> Result<()> {
        self.exec([
            "branch",
            "--quiet",
            "--track",
            branch.as_str(),
            &Rev::Origin(branch).spec(),
        ])
    }

    pub fn checkout(&self, _: &Lock, branch: &BranchName) -> Result<()> {
        self.exec(["checkout", "--quiet", branch.as_str()])
    }

    pub fn merge_ff_only(&self, _: &Lock, target: Rev<'_>) -> Result<()> {
        self.exec(["merge", "--ff-only", "--quiet", &target.spec()])
    }

    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        self.query(["config", "--get", key])
    }
}

fn parse_status(s: &str) -> Option<Status> {
    let mut st = Status {
        branch: None,
        head: None,
        upstream: Upstream::None,
        changes: Changes::default(),
    };
    let mut upstream_name = None;
    let mut ab = None;
    let mut fields = s.split('\0').filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        if let Some(h) = f.strip_prefix("# branch.") {
            let (key, value) = h.split_once(' ')?;
            match key {
                "oid" => st.head = value.parse().ok(),
                "head" if value != "(detached)" => st.branch = value.parse().ok(),
                "upstream" => upstream_name = Some(value.to_owned()),
                "ab" => {
                    let (a, b) = value.split_once(' ')?;
                    ab = Some((
                        a.strip_prefix('+')?.parse().ok()?,
                        b.strip_prefix('-')?.parse().ok()?,
                    ));
                }
                _ => {}
            }
        } else if f.starts_with('?') {
            st.changes.untracked += 1;
        } else if f.starts_with(['1', 'u']) {
            st.changes.tracked += 1;
        } else if f.starts_with('2') {
            // Renames and copies carry the original path as an extra field.
            st.changes.tracked += 1;
            fields.next();
        }
    }
    st.upstream = match (upstream_name, ab) {
        (None, _) => Upstream::None,
        (Some(name), None) => Upstream::Gone { name },
        (Some(name), Some((ahead, behind))) => Upstream::Set {
            name,
            ahead,
            behind,
        },
    };
    Some(st)
}

fn parse_worktrees(s: &str) -> Vec<WorktreeEntry> {
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
    out
}

fn failed(cmd: &Command, out: &Output) -> GitError {
    // Copy-pasteable, quoted like a shell would need it.
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    GitError::Failed {
        args: shell_words::join(args),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Gone vs set upstream decides whether removal measures against the
    // upstream or the default branch, so the parse matters.
    #[test]
    fn status_upstream_states() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let set = format!(
            "# branch.oid {sha}\0# branch.head feat/X-1\0# branch.upstream origin/feat/X-1\0# branch.ab +2 -0\0? new.txt\0"
        );
        let st = parse_status(&set);
        assert!(matches!(
            st.as_ref().map(|s| (&s.upstream, s.changes)),
            Some((
                Upstream::Set {
                    ahead: 2,
                    behind: 0,
                    ..
                },
                Changes {
                    tracked: 0,
                    untracked: 1
                }
            ))
        ));

        let gone = format!(
            "# branch.oid {sha}\0# branch.head feat/X-1\0# branch.upstream origin/feat/X-1\0"
        );
        assert!(matches!(
            parse_status(&gone).map(|s| s.upstream),
            Some(Upstream::Gone { .. })
        ));

        let detached = format!("# branch.oid {sha}\0# branch.head (detached)\0");
        assert!(matches!(
            parse_status(&detached).map(|s| (s.branch, s.upstream)),
            Some((None, Upstream::None))
        ));
    }
}
