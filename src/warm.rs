//! Warm build dirs: a new worktree gets `target/`, `node_modules/` (or what
//! `warm.dirs` says) from the most recently built worktree of the same repo,
//! so it does not start from a cold build.
//!
//! Files go through `std::fs::copy`, which clones on APFS (`fclonefileat`),
//! reflinks on btrfs/XFS (`copy_file_range`) and calls `CopyFileEx` on
//! Windows. Mtimes are set explicitly: Cargo decides freshness by them.

use std::path::{Path, PathBuf};
use std::time::Instant;

use filetime::FileTime;
use serde::Serialize;
use walkdir::WalkDir;

use crate::config::{Fallback, Warm};
use crate::names::DirName;
use crate::workspace::{Repo, canonical, par_map};

#[derive(Debug, Serialize)]
pub struct WarmReport {
    pub dir: DirName,
    #[serde(flatten)]
    pub outcome: WarmOutcome,
}

#[derive(Debug, Serialize)]
#[serde(tag = "warm", rename_all = "snake_case")]
pub enum WarmOutcome {
    Copied {
        from: PathBuf,
        mode: Mode,
        files: usize,
        bytes: u64,
        millis: u128,
        /// Symlinks that could not be recreated (Windows without the
        /// privilege to create them).
        #[serde(skip_serializing_if = "is_zero")]
        skipped_links: usize,
    },
    Skipped {
        why: Skip,
    },
    Failed {
        error: String,
    },
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Copy-on-write: instant, no extra disk until files change.
    Clone,
    /// A real copy; costs disk and time.
    FullCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// No other worktree of the repo has this dir.
    NoSource,
    /// The worktree has it already.
    Exists,
    /// No copy-on-write on this volume and `warm.fallback = "skip"`.
    NoCopyOnWrite,
}

/// Whether a new worktree gets warm build dirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmth {
    Warm,
    Cold,
}

/// Warms every configured dir of `dest`, a worktree of `repo`.
pub fn warm(repo: &Repo<'_>, config: &Warm, dest: &Path) -> Vec<WarmReport> {
    let Warm { dirs, fallback } = config;
    let fallback = *fallback;
    let sources = match sources(repo, dest) {
        Ok(s) => s,
        Err(e) => {
            return dirs
                .iter()
                .map(|d| WarmReport {
                    dir: d.clone(),
                    outcome: WarmOutcome::Failed {
                        error: e.to_string(),
                    },
                })
                .collect();
        }
    };
    dirs.iter()
        .map(|dir| WarmReport {
            dir: dir.clone(),
            outcome: warm_dir(&sources, dir, fallback, dest),
        })
        .collect()
}

/// The repo's checkout and its other worktrees.
fn sources(repo: &Repo<'_>, dest: &Path) -> Result<Vec<PathBuf>, crate::error::GitError> {
    let dest = canonical(dest);
    Ok(repo
        .git
        .worktrees()?
        .into_iter()
        .filter(|w| !w.prunable)
        .map(|w| canonical(&w.path))
        .filter(|p| *p != dest)
        .collect())
}

fn warm_dir(sources: &[PathBuf], dir: &DirName, fallback: Fallback, dest: &Path) -> WarmOutcome {
    let target = dest.join(dir);
    if target.exists() {
        return WarmOutcome::Skipped { why: Skip::Exists };
    }
    // Newest by the dir's own mtime, which changes whenever entries are added.
    let Some(from) = sources
        .iter()
        .map(|s| s.join(dir))
        .filter_map(|p| {
            Some((
                p.symlink_metadata()
                    .ok()
                    .filter(std::fs::Metadata::is_dir)?
                    .modified()
                    .ok()?,
                p,
            ))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
    else {
        return WarmOutcome::Skipped {
            why: Skip::NoSource,
        };
    };
    let mode = mode(&from, dest);
    if mode == Mode::FullCopy && fallback == Fallback::Skip {
        return WarmOutcome::Skipped {
            why: Skip::NoCopyOnWrite,
        };
    }
    let start = Instant::now();
    match copy_tree(&from, &target) {
        Ok(stats) => WarmOutcome::Copied {
            from,
            mode,
            files: stats.files,
            bytes: stats.bytes,
            millis: start.elapsed().as_millis(),
            skipped_links: stats.skipped_links,
        },
        Err(e) => WarmOutcome::Failed {
            error: e.to_string(),
        },
    }
}

#[derive(Debug, Default)]
struct Stats {
    files: usize,
    bytes: u64,
    skipped_links: usize,
}

/// Copies into a temp dir next to `target` and renames at the end, so a
/// crash or error never leaves a half-filled `target/` that later runs
/// would take for a warm cache.
fn copy_tree(from: &Path, target: &Path) -> std::io::Result<Stats> {
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".ws-warm");
    let tmp = PathBuf::from(tmp);
    if tmp.exists() {
        // Ours from an interrupted run.
        std::fs::remove_dir_all(&tmp)?;
    }
    let result =
        copy_into(from, &tmp).and_then(|stats| std::fs::rename(&tmp, target).map(|()| stats));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

fn copy_into(from: &Path, to: &Path) -> std::io::Result<Stats> {
    let mut stats = Stats::default();
    let mut dirs: Vec<(PathBuf, FileTime)> = Vec::new();
    let mut files: Vec<(PathBuf, PathBuf, FileTime, u64)> = Vec::new();
    for entry in WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(std::io::Error::other)?;
        let rel = entry
            .path()
            .strip_prefix(from)
            .map_err(std::io::Error::other)?;
        let dst = to.join(rel);
        let meta = entry.path().symlink_metadata()?;
        let mtime = FileTime::from_last_modification_time(&meta);
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir(&dst)?;
            dirs.push((dst, mtime));
        } else if ft.is_symlink() {
            if link(entry.path(), &dst).is_err() {
                stats.skipped_links += 1;
            }
        } else {
            files.push((entry.path().to_owned(), dst, mtime, meta.len()));
        }
    }
    for r in par_map(&files, |(src, dst, mtime, _)| {
        std::fs::copy(src, dst)?;
        filetime::set_file_mtime(dst, *mtime)
    }) {
        r?;
    }
    stats.files = files.len();
    stats.bytes = files.iter().map(|f| f.3).sum();
    // Children first: creating entries bumps a dir's mtime.
    for (dir, mtime) in dirs.iter().rev() {
        filetime::set_file_mtime(dir, *mtime)?;
    }
    Ok(stats)
}

#[cfg(unix)]
fn link(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
}

#[cfg(windows)]
fn link(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    if std::fs::metadata(src).is_ok_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

/// Whether `std::fs::copy` will clone here: a copy-on-write filesystem, and
/// source and destination on the same volume.
fn mode(from: &Path, dest: &Path) -> Mode {
    if same_volume(from, dest) && cow_filesystem(from) {
        Mode::Clone
    } else {
        Mode::FullCopy
    }
}

#[cfg(unix)]
fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.metadata(), b.metadata()) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_volume(_: &Path, _: &Path) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn cow_filesystem(path: &Path) -> bool {
    nix::sys::statfs::statfs(path).is_ok_and(|s| s.filesystem_type_name() == "apfs")
}

#[cfg(target_os = "linux")]
fn cow_filesystem(path: &Path) -> bool {
    use nix::sys::statfs::{BTRFS_SUPER_MAGIC, XFS_SUPER_MAGIC, statfs};
    statfs(path).is_ok_and(|s| [BTRFS_SUPER_MAGIC, XFS_SUPER_MAGIC].contains(&s.filesystem_type()))
}

/// Windows clones only on ReFS / Dev Drive, which we do not rely on.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cow_filesystem(_: &Path) -> bool {
    false
}
