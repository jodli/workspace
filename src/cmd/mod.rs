//! One module per command. Each returns a typed report; `main` serializes it.

pub mod add;
pub mod adopt;
pub mod create;
pub mod doctor;
pub mod drop;
pub mod list;
pub mod prune;
pub mod sync;

use serde::Serialize;

use crate::error::{Error, ErrorReport, Exit};

/// A command result. `exit` is `Incomplete` when part of the work was kept or
/// failed; the report says which part.
pub trait Report: Serialize {
    fn exit(&self) -> Exit {
        Exit::Ok
    }
}

/// An error as JSON: the whole output of a failed command, or one failed item
/// inside a report.
#[derive(Debug, Serialize)]
pub struct Failure {
    pub error: ErrorReport,
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self { error: e.report() }
    }
}
