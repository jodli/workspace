//! `ws.toml`, the per-workspace config. Its presence also marks the workspace
//! root.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, WorkspaceError};
use crate::names::{DirName, GitlabPath, Host};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub gitlab: Gitlab,
    #[serde(default)]
    pub warm: Warm,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gitlab {
    pub host: Host,
}

impl Gitlab {
    pub fn ssh_url(&self, path: &GitlabPath) -> String {
        format!("git@{}:{path}.git", self.host)
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
