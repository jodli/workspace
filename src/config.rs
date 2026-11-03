//! `ws.toml`, the per-workspace config. Its presence also marks the workspace
//! root.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, WorkspaceError};
use crate::names::{BranchName, DirName, Host, RemotePath};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub forge: Forge,
    #[serde(default)]
    pub warm: Warm,
}

/// Where the repos are hosted. ws never talks to the forge itself; it only
/// builds clone URLs and the commands an agent runs to check merges.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    pub kind: ForgeKind,
    pub host: Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForgeKind {
    Gitlab,
    Github,
}

/// A command that lists the merged pull/merge requests of one branch, as
/// JSON, and the field in each entry that holds the request's head sha.
#[derive(Debug, Clone, Serialize)]
pub struct MergedQuery {
    pub command: String,
    pub head_sha_field: &'static str,
}

impl Forge {
    /// Both forges take the same ssh form.
    pub fn ssh_url(&self, path: &RemotePath) -> String {
        format!("git@{}:{path}.git", self.host)
    }

    /// The repo is passed with its host, so the command works outside a
    /// checkout and against self-hosted instances.
    pub fn merged_query(&self, path: &RemotePath, branch: &BranchName) -> MergedQuery {
        let repo = format!("{}/{path}", self.host);
        let b = branch.as_str();
        let (args, head_sha_field): (Vec<&str>, _) = match self.kind {
            ForgeKind::Gitlab => (
                vec![
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
                ],
                "sha",
            ),
            ForgeKind::Github => (
                vec![
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
                ],
                "headRefOid",
            ),
        };
        MergedQuery {
            command: shell_words::join(args),
            head_sha_field,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Warm {
    pub dirs: Vec<DirName>,
    pub fallback: Fallback,
}

impl Default for Warm {
    fn default() -> Self {
        Self {
            dirs: vec![DirName::known("target"), DirName::known("node_modules")],
            fallback: Fallback::default(),
        }
    }
}

/// What the warm copy does on a volume without copy-on-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fallback {
    /// Full copy. Costs disk and time, still beats a cold build.
    #[default]
    Copy,
    Skip,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(Error::io(path))?;
        parse_toml(path, &text)
    }
}

/// TOML from a workspace file, with the file named in the error.
pub fn parse_toml<T: serde::de::DeserializeOwned>(path: &Path, text: &str) -> Result<T, Error> {
    toml::from_str(text).map_err(|e| {
        WorkspaceError::Config {
            path: path.to_owned(),
            message: e.message().to_owned(),
        }
        .into()
    })
}
