//! `ws`: a multi-repo worktree workspace, driven by coding agents.
//!
//! ```text
//! <root>/ws.toml          config
//! <root>/repos.tsv        roster
//! <root>/repos/<repo>     canonical checkout, default branch, never worked in
//! <root>/spaces/<space>/  one worktree per involved repo, plus SPACE.md
//! ```

pub mod branch;
pub mod cmd;
pub mod config;
pub mod error;
pub mod git;
pub mod names;
pub mod roster;
pub mod space;
pub mod workspace;
