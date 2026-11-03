//! Warm build dirs: a new worktree gets `target/`, `node_modules/` (or what
//! `warm.dirs` says) from the most recently built worktree of the same repo,
//! so it does not start from a cold build.
//!
//! Files go through `std::fs::copy`, which clones on APFS (`fclonefileat`),
//! reflinks on btrfs/XFS (`copy_file_range`) and calls `CopyFileEx` on
//! Windows. Mtimes are set explicitly: Cargo decides freshness by them.

use std::path::{Path, PathBuf};

use filetime::FileTime;
use serde::Serialize;
use walkdir::WalkDir;

use crate::cmd::Failure;
use crate::config::{Fallback, Warm};
use crate::error::Error;
use crate::names::DirName;
use crate::workspace::{Repo, canonical, par_map};

/// Whether a new worktree gets warm build dirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmth {
    Warm,
    Cold,
}

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
        /// Symlinks Windows refused to create (needs Developer Mode).
        skipped_links: usize,
    },
    Skipped {
        why: Skip,
    },
    Failed(Failure),
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

/// Warms every configured dir of `dest`, a worktree of `repo`.
pub fn warm(repo: &Repo<'_>, config: &Warm, dest: &Path) -> Vec<WarmReport> {
    let sources = sources(repo, dest);
    config
        .dirs
        .iter()
        .map(|dir| WarmReport {
            dir: dir.clone(),
            outcome: match &sources {
                Ok(s) => warm_dir(s, dir, config.fallback, dest),
                Err(e) => WarmOutcome::Failed(Failure { error: e.report() }),
            },
        })
        .collect()
}

/// The repo's checkout and its other worktrees.
fn sources(repo: &Repo<'_>, dest: &Path) -> Result<Vec<PathBuf>, Error> {
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
    let newest = sources
        .iter()
        .map(|s| s.join(dir))
        .filter_map(|p| {
            let meta = p
                .symlink_metadata()
                .ok()
                .filter(std::fs::Metadata::is_dir)?;
            Some((meta.modified().ok()?, p))
        })
        .max_by_key(|(t, _)| *t);
    let Some((_, from)) = newest else {
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
    match copy_tree(&from, &target) {
        Ok(c) => WarmOutcome::Copied {
            from,
            mode,
            files: c.files,
            bytes: c.bytes,
            skipped_links: c.skipped_links,
        },
        Err(e) => WarmOutcome::Failed(Failure { error: e.report() }),
    }
}

#[derive(Debug)]
struct Copied {
    files: usize,
    bytes: u64,
    skipped_links: usize,
}

struct FileJob {
    src: PathBuf,
    dst: PathBuf,
    mtime: FileTime,
}

/// Copies into a temp dir next to `target` and renames at the end, so a
/// crash or error never leaves a half-filled `target/` that later runs
/// would take for a warm cache.
fn copy_tree(from: &Path, target: &Path) -> Result<Copied, Error> {
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".ws-warm");
    let tmp = PathBuf::from(tmp);
    if tmp.exists() {
        // Ours, from an interrupted run.
        std::fs::remove_dir_all(&tmp).map_err(Error::io(&tmp))?;
    }
    let result = copy_into(from, &tmp).and_then(|c| {
        std::fs::rename(&tmp, target)
            .map(|()| c)
            .map_err(Error::io(target))
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

fn copy_into(from: &Path, to: &Path) -> Result<Copied, Error> {
    let mut dirs: Vec<(PathBuf, FileTime)> = Vec::new();
    let mut files: Vec<FileJob> = Vec::new();
    let mut skipped_links = 0;
    for entry in WalkDir::new(from).follow_links(false) {
        let entry = entry.map_err(|e| Error::io(from)(e.into()))?;
        let rel = entry.path().strip_prefix(from).unwrap_or(entry.path());
        let dst = to.join(rel);
        let ft = entry.file_type();
        if ft.is_symlink() {
            link(entry.path(), &dst, &mut skipped_links)?;
            continue;
        }
        // walkdir's metadata is the lstat it already did (and free on Windows).
        let meta = entry
            .metadata()
            .map_err(|e| Error::io(entry.path())(e.into()))?;
        let mtime = FileTime::from_last_modification_time(&meta);
        if ft.is_dir() {
            std::fs::create_dir(&dst).map_err(Error::io(&dst))?;
            dirs.push((dst, mtime));
        } else {
            files.push(FileJob {
                src: entry.path().to_owned(),
                dst,
                mtime,
            });
        }
    }
    let bytes = par_map(&files, |job| {
        let n = std::fs::copy(&job.src, &job.dst).map_err(Error::io(&job.src))?;
        filetime::set_file_mtime(&job.dst, job.mtime).map_err(Error::io(&job.dst))?;
        Ok::<_, Error>(n)
    })
    .into_iter()
    .sum::<Result<u64, Error>>()?;
    // Children first: creating entries bumps a dir's mtime.
    for (dir, mtime) in dirs.iter().rev() {
        filetime::set_file_mtime(dir, *mtime).map_err(Error::io(dir))?;
    }
    Ok(Copied {
        files: files.len(),
        bytes,
        skipped_links,
    })
}

#[cfg(unix)]
fn link(src: &Path, dst: &Path, _: &mut usize) -> Result<(), Error> {
    let target = std::fs::read_link(src).map_err(Error::io(src))?;
    std::os::unix::fs::symlink(target, dst).map_err(Error::io(dst))
}

/// Creating symlinks needs a privilege on Windows. Without it the link is
/// counted and skipped rather than failing the whole copy.
#[cfg(windows)]
fn link(src: &Path, dst: &Path, skipped: &mut usize) -> Result<(), Error> {
    let target = std::fs::read_link(src).map_err(Error::io(src))?;
    let made = if std::fs::metadata(src).is_ok_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    };
    if made.is_err() {
        *skipped += 1;
    }
    Ok(())
}

/// Whether `std::fs::copy` will clone: a copy-on-write filesystem, with
/// source and destination on the same volume.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn mode(from: &Path, dest: &Path) -> Mode {
    use std::os::unix::fs::MetadataExt;
    let same_volume =
        matches!((from.metadata(), dest.metadata()), (Ok(a), Ok(b)) if a.dev() == b.dev());
    if same_volume && cow_filesystem(from) {
        Mode::Clone
    } else {
        Mode::FullCopy
    }
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

/// Windows clones only on ReFS / Dev Drive, which ws does not rely on.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn mode(_: &Path, _: &Path) -> Mode {
    Mode::FullCopy
}
