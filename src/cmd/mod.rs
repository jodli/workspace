//! One module per command. Each returns a typed report; `main` serializes it.

pub mod add;
pub mod adopt;
pub mod create;
pub mod doctor;
pub mod list;
pub mod sync;

use std::collections::HashMap;

use serde::Serialize;

use crate::error::{Error, ErrorReport, Exit};
use crate::names::{BranchName, RepoName};
use crate::workspace::Repo;

/// A command result. `exit` is `Incomplete` when part of the work was kept or
/// failed; the report says which part.
pub trait Report: Serialize {
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

/// Default branch per repo, asked once per run.
#[derive(Debug, Default)]
pub struct Defaults(HashMap<RepoName, BranchName>);

impl Defaults {
    pub fn of(&mut self, repo: &Repo<'_>) -> Result<BranchName, Error> {
        if let Some(b) = self.0.get(&repo.entry.name) {
            return Ok(b.clone());
        }
        let b = repo.git.default_branch()?;
        self.0.insert(repo.entry.name.clone(), b.clone());
        Ok(b)
    }
}

/// A per-item failure inside an otherwise successful report.
#[derive(Debug, Serialize)]
pub struct Failure {
    pub error: ErrorReport,
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self { error: e.report() }
    }
}
