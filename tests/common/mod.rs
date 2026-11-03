//! A throwaway workspace with one repo whose origin is a local bare repo.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use ws::workspace::Workspace;

pub type TestResult<T = ()> = Result<T, Box<dyn Error>>;

pub const REPO: &str = "app";

pub struct Fixture {
    _tmp: TempDir,
    pub root: PathBuf,
    pub origin: PathBuf,
}

pub fn git(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn commit(dir: &Path, file: &str, content: &str) -> TestResult<String> {
    std::fs::write(dir.join(file), content)?;
    git(dir, &["add", file])?;
    git(dir, &["commit", "--quiet", "-m", file])?;
    git(dir, &["rev-parse", "HEAD"])
}

impl Fixture {
    pub fn new() -> TestResult<Self> {
        let tmp = TempDir::new()?;
        let base = dunce::canonicalize(tmp.path())?;
        let seed = base.join("seed");
        let origin = base.join("origin.git");
        let root = base.join("ws");
        std::fs::create_dir_all(&seed)?;
        git(&seed, &["init", "--quiet", "-b", "main"])?;
        commit(&seed, "README", "hello")?;
        git(
            &base,
            &["init", "--quiet", "--bare", "-b", "main", "origin.git"],
        )?;
        git(
            &seed,
            &["push", "--quiet", origin.to_str().ok_or("path")?, "main"],
        )?;

        std::fs::create_dir_all(root.join("repos"))?;
        std::fs::write(
            root.join("ws.toml"),
            "[gitlab]\nhost = \"gitlab.example.com\"\n",
        )?;
        std::fs::write(root.join("repos.tsv"), format!("{REPO}\tgroup/{REPO}\n"))?;
        git(
            &root.join("repos"),
            &["clone", "--quiet", origin.to_str().ok_or("path")?, REPO],
        )?;
        Ok(Self {
            _tmp: tmp,
            root,
            origin,
        })
    }

    pub fn ws(&self) -> TestResult<Workspace> {
        Ok(Workspace::open(&self.root)?)
    }

    pub fn worktree(&self, space: &str) -> PathBuf {
        self.root.join("spaces").join(space).join(REPO)
    }
}
