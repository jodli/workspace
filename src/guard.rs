//! `ws guard`: a Claude Code `PreToolUse` hook that keeps agents on the `ws`
//! path. It denies writes into `repos/` and raw worktree management, and
//! names the `ws` command to use instead.
//!
//! A guardrail, not a sandbox: it reads commands the way an agent writes
//! them (`git -C <path> ...`, `cd <path> && ...`). Anything it cannot parse
//! is allowed rather than blocking the agent on a guard bug.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::workspace::{canonical, find_root};

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

const USE_SPACE: &str = "repos/ holds canonical checkouts and is read-only. Work in a space: \
     `ws start <KEY> --issue <type> <repo>` or `ws spike <name> <repo>`; \
     `ws adopt <space> <repo> <branch>` for an existing branch.";
const USE_WS_WORKTREES: &str = "worktrees are managed by ws: `ws spike`/`ws start`/`ws adopt` create them, \
     `ws drop`/`ws prune` remove them. `git worktree list` is fine.";
const USE_DROP: &str = "spaces and worktrees are removed with `ws drop <space>` (or `ws prune`), which \
     refuses to lose work. Deleting them by hand leaves stale git state.";

/// Git subcommands that only read. Everything else is a write as far as
/// `repos/` is concerned.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "show",
    "diff",
    "grep",
    "blame",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "rev-parse",
    "rev-list",
    "describe",
    "cat-file",
    "shortlog",
    "reflog",
    "for-each-ref",
    "show-ref",
    "merge-base",
    "name-rev",
    "check-ignore",
    "count-objects",
    "whatchanged",
    "fetch",
    "help",
    "version",
];

pub fn decide(input: &HookInput) -> Decision {
    let cwd = input.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
    match input.tool_name.as_str() {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = ["file_path", "notebook_path"]
                .iter()
                .find_map(|k| input.tool_input.get(k)?.as_str());
            match path {
                Some(p) if in_repos(&resolve(&cwd, Path::new(p))) => {
                    Decision::Deny(USE_SPACE.into())
                }
                _ => Decision::Allow,
            }
        }
        "Bash" => input
            .tool_input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map_or(Decision::Allow, |c| bash(&cwd, c)),
        _ => Decision::Allow,
    }
}

fn bash(cwd: &Path, command: &str) -> Decision {
    let Ok(words) = shell_words::split(command) else {
        return Decision::Allow;
    };
    let mut cwd = cwd.to_owned();
    // Segments between shell operators, each one simple command.
    for segment in words.split(|w| matches!(w.as_str(), "&&" | "||" | ";" | "|" | "&")) {
        match segment {
            [cd, dir, ..] if cd == "cd" => cwd = resolve(&cwd, Path::new(dir)),
            [git, args @ ..] if git == "git" => {
                if let Some(reason) = git_command(&cwd, args) {
                    return Decision::Deny(reason.into());
                }
            }
            [rm, args @ ..] if rm == "rm" => {
                if let Some(reason) = rm_command(&cwd, args) {
                    return Decision::Deny(reason.into());
                }
            }
            _ => {}
        }
    }
    Decision::Allow
}

fn git_command(cwd: &Path, args: &[String]) -> Option<&'static str> {
    let mut dir = cwd.to_owned();
    let mut rest = args.iter();
    let sub = loop {
        match rest.next()?.as_str() {
            "-C" => dir = resolve(&dir, Path::new(rest.next()?)),
            "-c" => {
                rest.next()?;
            }
            a if a.starts_with('-') => {}
            sub => break sub,
        }
    };
    let rest: Vec<&str> = rest.map(String::as_str).collect();
    if sub == "worktree" {
        return (rest.first() != Some(&"list") && find_root(&dir).is_some())
            .then_some(USE_WS_WORKTREES);
    }
    if !in_repos(&dir) || READ_ONLY_GIT.contains(&sub) {
        return None;
    }
    let read_only = match sub {
        "branch" => branch_lists(&rest),
        "config" => rest
            .iter()
            .any(|a| matches!(*a, "--get" | "--get-all" | "--get-regexp" | "--list" | "-l")),
        "remote" => rest.is_empty() || rest == ["-v"] || rest.first() == Some(&"show"),
        "stash" => matches!(rest.first(), Some(&"list" | &"show")),
        _ => false,
    };
    (!read_only).then_some(USE_SPACE)
}

/// `git branch` with only listing flags. Any positional argument names a
/// branch to create, any of the editing flags changes one.
fn branch_lists(args: &[&str]) -> bool {
    const TAKES_VALUE: &[&str] = &[
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--format",
        "--sort",
    ];
    const EDITS: &[&str] = &[
        "-d",
        "-D",
        "-m",
        "-M",
        "-c",
        "-C",
        "-f",
        "-u",
        "--delete",
        "--move",
        "--copy",
        "--force",
        "--set-upstream-to",
        "--unset-upstream",
        "--edit-description",
        "--track",
    ];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if EDITS.contains(a) || a.starts_with("--set-upstream-to=") {
            return false;
        }
        if TAKES_VALUE.contains(a) {
            it.next();
        } else if !a.starts_with('-') {
            return false;
        }
    }
    true
}

/// Recursive removal of `repos/` content, `spaces/`, a space or a worktree
/// root. Deleting inside a worktree (`rm -rf target`) is normal work.
fn rm_command(cwd: &Path, args: &[String]) -> Option<&'static str> {
    let recursive = args.iter().any(|a| {
        a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R']) || a == "--recursive"
    });
    if !recursive {
        return None;
    }
    for target in args.iter().filter(|a| !a.starts_with('-')) {
        let path = resolve(cwd, Path::new(target));
        if in_repos(&path) {
            return Some(USE_SPACE);
        }
        let Some(root) = find_root(&path) else {
            continue;
        };
        if let Ok(rel) = path.strip_prefix(root.join("spaces"))
            && rel.components().count() <= 2
        {
            return Some(USE_DROP);
        }
    }
    None
}

/// Under `<workspace>/repos/`.
fn in_repos(path: &Path) -> bool {
    find_root(path).is_some_and(|root| path.strip_prefix(root.join("repos")).is_ok())
}

/// Absolute and without `.`/`..`, resolving symlinks where the path exists so
/// it compares equal to the canonical workspace root.
fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    // Canonicalize the longest existing prefix; new files do not exist yet.
    let existing = out.ancestors().find(|a| a.exists()).map(Path::to_path_buf);
    match existing {
        Some(base) => {
            let rest = out
                .strip_prefix(&base)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            canonical(&base).join(rest)
        }
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::io::Result<(tempfile::TempDir, PathBuf)> {
        let tmp = tempfile::TempDir::new()?;
        let root = canonical(tmp.path());
        std::fs::write(root.join("ws.toml"), "")?;
        std::fs::create_dir_all(root.join("repos/app"))?;
        std::fs::create_dir_all(root.join("spaces/s/app"))?;
        Ok((tmp, root))
    }

    fn run(root: &Path, tool: &str, input: serde_json::Value) -> Decision {
        decide(&HookInput {
            tool_name: tool.into(),
            tool_input: input,
            cwd: Some(root.to_owned()),
        })
    }

    fn bash_in(root: &Path, cmd: &str) -> Decision {
        run(root, "Bash", serde_json::json!({ "command": cmd }))
    }

    #[test]
    fn denies_writes_into_repos_and_allows_reads() -> std::io::Result<()> {
        let (_tmp, root) = workspace()?;
        let denied = [
            "git -C repos/app checkout -b x",
            "cd repos/app && git commit -am wip",
            "git -C repos/app branch feature",
            "git worktree add spaces/s/other x",
            "rm -rf spaces/s",
            "rm -r repos/app/src",
        ];
        for cmd in denied {
            assert!(matches!(bash_in(&root, cmd), Decision::Deny(_)), "{cmd}");
        }
        let allowed = [
            "git -C repos/app log --oneline -5",
            "git -C repos/app branch -a --contains abc",
            "git -C repos/app status",
            "git -C spaces/s/app commit -am wip",
            "git worktree list",
            "rm -rf spaces/s/app/target",
            "cat repos/app/README",
        ];
        for cmd in allowed {
            assert_eq!(bash_in(&root, cmd), Decision::Allow, "{cmd}");
        }
        let edit = |p: &Path| run(&root, "Edit", serde_json::json!({ "file_path": p }));
        assert!(matches!(
            edit(&root.join("repos/app/src/new.rs")),
            Decision::Deny(_)
        ));
        assert_eq!(edit(&root.join("spaces/s/app/src/new.rs")), Decision::Allow);
        Ok(())
    }
}
