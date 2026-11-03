use std::process::ExitCode;

use clap::{ColorChoice, Parser, Subcommand};
use serde::Serialize;

use ws::branch::{ChangeType, IssueType, PrefixStyle};
use ws::cmd::prune::Apply;
use ws::cmd::{self, Failure, Report};
use ws::error::{Error, Exit, InputError};
use ws::guard;
use ws::names::{BranchName, GitlabPath, JiraKey, RepoName, SpaceName};
use ws::removal::{MergedProof, Safety};
use ws::warm::Warmth;
use ws::workspace::Workspace;

/// Multi-repo worktree workspace for coding agents.
///
/// Every command prints one JSON object on stdout. Exit 0: done. Exit 1: done
/// in part, the result says what was kept or failed. Exit 2-6: an error object
/// `{"error": {kind, message, hint}}` (2 input, 3 workspace, 4 git, 5 busy,
/// 6 io).
#[derive(Debug, Parser)]
#[command(name = "ws", version, color = ColorChoice::Never)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Args)]
struct WarmArgs {
    /// Skip copying build dirs (warm.dirs) from another worktree of the repo.
    #[arg(long)]
    cold: bool,
}

impl From<WarmArgs> for Warmth {
    fn from(a: WarmArgs) -> Self {
        if a.cold { Self::Cold } else { Self::Warm }
    }
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Clone missing roster repos, fetch, fast-forward default branches.
    Sync,
    /// All repos and spaces with their git state.
    #[command(alias = "ls")]
    List,
    /// One space with its git state.
    Status { space: SpaceName },
    /// New spike space: branch `spike/<name>` in each repo.
    Spike {
        name: SpaceName,
        #[arg(required = true)]
        repos: Vec<RepoName>,
        #[command(flatten)]
        warm: WarmArgs,
    },
    /// New ticket space `spaces/<KEY>`: branch `<prefix>/<KEY>` in each repo,
    /// prefix from the issue type and the repo's style.
    Start {
        key: JiraKey,
        /// Jira issue type. For a sub-task, pass the parent's type.
        #[arg(long, value_enum)]
        issue: IssueType,
        /// Required for tasks; must match for bugs (fix) and stories (feat).
        #[arg(long, value_enum)]
        change: Option<ChangeType>,
        #[arg(required = true)]
        repos: Vec<RepoName>,
        #[command(flatten)]
        warm: WarmArgs,
    },
    /// Put an existing branch (local or on origin) into a space.
    Adopt {
        space: SpaceName,
        repo: RepoName,
        branch: BranchName,
        #[command(flatten)]
        warm: WarmArgs,
    },
    /// Add GitLab projects to the roster and clone them.
    Add {
        #[arg(required = true)]
        paths: Vec<GitlabPath>,
        /// The repos use feature/ and bugfix/ instead of feat/ and fix/.
        #[arg(long)]
        gitflow: bool,
    },
    /// Remove a space's worktrees that are safe to remove, then the space.
    /// Clean worktrees with nothing unpushed always qualify.
    #[command(alias = "rm")]
    Drop {
        space: SpaceName,
        /// `<repo>=<sha>`: the MR of this repo's branch is merged at this head
        /// sha (from GitLab). Lets squash-merged branches go.
        #[arg(long = "merged", value_name = "REPO=SHA")]
        merged: Vec<MergedProof>,
        /// Remove even with uncommitted or unpushed work. Loses it.
        #[arg(long)]
        force: bool,
    },
    /// Report which spaces are finished; with --yes, remove them.
    Prune {
        #[arg(long = "merged", value_name = "REPO=SHA")]
        merged: Vec<MergedProof>,
        #[arg(long)]
        yes: bool,
    },
    /// Check git, GitLab reachability and the workspace invariants.
    Doctor,
    /// Claude Code `PreToolUse` hook: reads the hook JSON on stdin, denies
    /// writes into repos/ and raw worktree management. Allows on any doubt.
    Guard,
}

fn print_json(value: &impl Serialize) {
    match serde_json::to_string(value) {
        Ok(s) => println!("{s}"),
        // Only possible for non-string map keys, which no report has.
        Err(e) => {
            println!(r#"{{"error":{{"kind":"io","message":"cannot serialize result: {e}"}}}}"#);
        }
    }
}

fn emit(report: &impl Report) -> Exit {
    print_json(report);
    report.exit()
}

/// Prints a deny decision, or nothing to let the call through.
fn guard() -> Exit {
    let input = std::io::read_to_string(std::io::stdin()).ok();
    let decision = input
        .as_deref()
        .and_then(|s| serde_json::from_str::<guard::HookInput>(s).ok())
        .map_or(guard::Decision::Allow, |i| guard::decide(&i));
    if let guard::Decision::Deny(reason) = decision {
        print_json(&serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }));
    }
    Exit::Ok
}

fn run(cmd: Cmd) -> Result<Exit, Error> {
    // The hook runs for every tool call, inside or outside a workspace.
    if matches!(cmd, Cmd::Guard) {
        return Ok(guard());
    }
    let mut ws = Workspace::discover()?;
    Ok(match cmd {
        Cmd::Sync => emit(&cmd::sync::sync(&ws)?),
        Cmd::List => emit(&cmd::list::list(&ws)?),
        Cmd::Status { space } => emit(&cmd::list::status(&ws, &space)?),
        Cmd::Spike { name, repos, warm } => {
            emit(&cmd::create::spike(&ws, name, &repos, warm.into())?)
        }
        Cmd::Start {
            key,
            issue,
            change,
            repos,
            warm,
        } => emit(&cmd::create::start(
            &ws,
            key,
            issue,
            change,
            &repos,
            warm.into(),
        )?),
        Cmd::Adopt {
            space,
            repo,
            branch,
            warm,
        } => emit(&cmd::adopt::adopt(&ws, space, &repo, branch, warm.into())?),
        Cmd::Add { paths, gitflow } => {
            let style = if gitflow {
                PrefixStyle::Gitflow
            } else {
                PrefixStyle::Conventional
            };
            emit(&cmd::add::add(&mut ws, &paths, style)?)
        }
        Cmd::Drop {
            space,
            merged,
            force,
        } => {
            let safety = if force {
                Safety::Forced
            } else {
                Safety::Checked
            };
            emit(&cmd::drop::drop(&ws, &space, &merged, safety)?)
        }
        Cmd::Prune { merged, yes } => {
            let apply = if yes { Apply::Remove } else { Apply::DryRun };
            emit(&cmd::prune::prune(&ws, &merged, apply)?)
        }
        Cmd::Doctor => emit(&cmd::doctor::doctor(&ws)?),
        Cmd::Guard => guard(),
    })
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => {
            // --help and --version: plain text is what the caller asked for.
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let err = Error::from(InputError::Usage(e.render().to_string().trim().to_owned()));
            print_json(&Failure::from(err));
            return Exit::Input.into();
        }
    };
    match run(cli.cmd) {
        Ok(exit) => exit.into(),
        Err(e) => {
            let exit = e.exit();
            print_json(&Failure::from(e));
            exit.into()
        }
    }
}
