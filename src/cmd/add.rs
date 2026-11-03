//! `ws add <remote path>...`: put repos in the roster and clone them.

use serde::Serialize;

use super::Report;
use super::sync::{SyncReport, sync_entries};
use crate::branch::PrefixStyle;
use crate::error::{Error, Exit, InputError};
use crate::names::{RemotePath, RepoName};
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
    paths: &[RemotePath],
    style: PrefixStyle,
) -> Result<AddReport, Error> {
    let lock = ws.lock()?;
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
            remote: path.clone(),
            style,
        };
        ws.roster.append(entry.clone())?;
        added.push(entry);
    }
    let entries: Vec<&RepoEntry> = added.iter().collect();
    let sync = sync_entries(ws, &lock, &entries)?;
    Ok(AddReport {
        added,
        already,
        sync,
    })
}
