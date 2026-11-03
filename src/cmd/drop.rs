//! `ws drop <space>`: remove a space's worktrees, but only those proven safe.
//! The space itself goes once nothing is left in it; `SPACE.md` is archived.

use std::path::PathBuf;

use serde::Serialize;

use super::Report;
use crate::error::{Error, Exit, InputError};
use crate::names::SpaceName;
use crate::removal::{
    FetchError, MergedProof, Safety, SpaceRemoval, evaluate_all, refresh, remove_space,
};
use crate::space;
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
pub struct DropReport {
    pub space: SpaceName,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fetch_errors: Vec<FetchError>,
    /// Directories that are not worktrees. They keep the space alive.
    pub strays: Vec<PathBuf>,
    #[serde(flatten)]
    pub removal: SpaceRemoval,
}

impl Report for DropReport {
    fn exit(&self) -> Exit {
        Exit::incomplete_if(!self.removal.space_removed)
    }
}

pub fn drop(
    ws: &Workspace,
    name: &SpaceName,
    proofs: &[MergedProof],
    safety: Safety,
) -> Result<DropReport, Error> {
    let lock = ws.lock()?;
    let scan = space::scan_one(ws, name)?;
    if let Some(p) = proofs
        .iter()
        .find(|p| !scan.worktrees.iter().any(|f| f.repo == p.repo))
    {
        return Err(InputError::MergedUnused(p.repo.clone()).into());
    }
    let fetch_errors = refresh(ws, &lock, scan.worktrees.iter().map(|f| &f.repo));
    let evaluated = evaluate_all(ws, &scan.worktrees.iter().collect::<Vec<_>>(), proofs)?;
    let removal = remove_space(
        ws,
        &lock,
        &scan.name,
        evaluated,
        scan.missing,
        !scan.strays.is_empty(),
        safety,
    )?;
    Ok(DropReport {
        space: scan.name,
        fetch_errors,
        strays: scan.strays,
        removal,
    })
}
