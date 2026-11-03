//! `ws add <gitlab path>...`: put repos in the roster and clone them.

use serde::Serialize;

use super::Report;
use super::sync::{SyncReport, sync_entries};
use crate::branch::PrefixStyle;
use crate::error::{Error, Exit, InputError};
use crate::names::{GitlabPath, RepoName};
use crate::roster::RepoEntry;
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
pub struct AddReport {
    pub added: Vec<RepoEntry>,
    pub already: Vec<RepoName>,
    pub sync: SyncReport,
}

impl Report for AddReport {
    fn exit(&self) -> Exit {
        self.sync.exit()
    }
}

pub fn add(
    ws: &mut Workspace,
    paths: &[GitlabPath],
    style: PrefixStyle,
) -> Result<AddReport, Error> {
    let _lock = ws.lock()?;
    let mut added = Vec::new();
    let mut already = Vec::new();
    for path in paths {
        if let Some(e) = ws.roster.by_path(path) {
            already.push(e.name.clone());
            continue;
        }
        let name = ws
            .roster
            .free_name(path)
            .ok_or_else(|| InputError::Usage(format!("no free local name for '{path}'")))?;
        let entry = RepoEntry {
            name,
            gitlab: path.clone(),
            style,
        };
        ws.roster.append(entry.clone())?;
        added.push(entry);
    }
    let names: Vec<&RepoName> = added.iter().map(|e| &e.name).collect();
    let sync = sync_entries(ws, ws.roster.iter().filter(|e| names.contains(&&e.name)))?;
    Ok(AddReport {
        added,
        already,
        sync,
    })
}
