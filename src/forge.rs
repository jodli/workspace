//! Where the repos are hosted. ws never talks to the forge itself; it builds
//! clone URLs and the command an agent runs to see whether a branch's
//! pull/merge request was merged. All per-forge knowledge lives here.

use serde::Deserialize;

use crate::names::{BranchName, Host, RemotePath};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    pub kind: ForgeKind,
    pub host: Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForgeKind {
    Gitlab,
    Github,
}

impl Forge {
    /// Both forges serve git over ssh as `git@<host>`, on this port.
    pub const SSH_PORT: u16 = 22;

    pub fn ssh_url(&self, path: &RemotePath) -> String {
        format!("git@{}:{path}.git", self.host)
    }

    /// A command printing the head sha of every merged request from
    /// `branch`, one per line. The repo carries its host, so it works outside
    /// a checkout and against self-hosted instances.
    pub fn merged_heads(&self, path: &RemotePath, branch: &BranchName) -> String {
        let repo = format!("{}/{path}", self.host);
        let b = branch.as_str();
        let args: &[&str] = match self.kind {
            ForgeKind::Gitlab => &[
                "glab",
                "mr",
                "list",
                "-R",
                &repo,
                "--source-branch",
                b,
                "--merged",
                "-F",
                "json",
                "--jq",
                ".[].sha",
            ],
            ForgeKind::Github => &[
                "gh",
                "pr",
                "list",
                "--repo",
                &repo,
                "--head",
                b,
                "--state",
                "merged",
                "--json",
                "headRefOid",
                "--jq",
                ".[].headRefOid",
            ],
        };
        shell_words::join(args)
    }
}
