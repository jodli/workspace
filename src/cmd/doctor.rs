//! `ws doctor`: everything that has to hold before the other commands work.

use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;

use super::{Defaults, Report};
use crate::error::{Error, Exit};
use crate::git::Git;
use crate::workspace::Workspace;

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
}

impl Report for DoctorReport {
    fn exit(&self) -> Exit {
        if self.checks.iter().all(|c| c.ok) {
            Exit::Ok
        } else {
            Exit::Incomplete
        }
    }
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>, hint: &'static str) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
        hint: (!ok).then_some(hint),
    }
}

pub fn doctor(ws: &Workspace) -> Result<DoctorReport, Error> {
    let mut checks = Vec::new();
    let host = ws.config.gitlab.host.as_str();

    checks.push(match Git::version() {
        Ok(v) => check("git", true, v, ""),
        Err(e) => check(
            "git",
            false,
            e.to_string(),
            "install git and put it on PATH",
        ),
    });

    checks.extend(network(host));

    let missing: Vec<String> = ws
        .roster
        .iter()
        .filter(|e| ws.repo(e).is_none())
        .map(|e| e.name.to_string())
        .collect();
    checks.push(check(
        "repos_cloned",
        missing.is_empty(),
        if missing.is_empty() {
            format!("{} repos", ws.roster.iter().count())
        } else {
            format!("missing: {}", missing.join(", "))
        },
        "`ws sync` clones them",
    ));

    let mut defaults = Defaults::default();
    let mut off: Vec<String> = Vec::new();
    for repo in ws.repos() {
        let default = defaults.of(&repo)?;
        let on_default = repo.git.head_branch()?.as_ref() == Some(&default);
        if !on_default || repo.git.changes()?.tracked > 0 {
            off.push(repo.entry.name.to_string());
        }
    }
    checks.push(check(
        "repos_canonical",
        off.is_empty(),
        if off.is_empty() {
            "all on a clean default branch".to_owned()
        } else {
            format!("not clean on default: {}", off.join(", "))
        },
        "move the work into a space with `ws adopt`, then the repo is restored",
    ));

    let legacy = ws.root.join("spikes");
    checks.push(check(
        "no_legacy_spikes_dir",
        !legacy.exists(),
        legacy.display().to_string(),
        "move each worktree with `git worktree move` into spaces/, then remove spikes/",
    ));

    if cfg!(windows) {
        let on = Git::at(&ws.root)
            .config_get("core.longpaths")
            .ok()
            .flatten()
            .is_some_and(|v| v == "true");
        checks.push(check(
            "git_longpaths",
            on,
            "core.longpaths",
            "git config --global core.longpaths true",
        ));
    }

    Ok(DoctorReport { checks })
}

/// Reachability first: without it the ssh check would only time out.
fn network(host: &str) -> Vec<Check> {
    let reachable = (host, 22)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_ok());
    let mut checks = vec![check(
        "gitlab_reachable",
        reachable,
        format!("{host}:22"),
        "check network access to gitlab.host in ws.toml",
    )];

    if reachable {
        let ssh = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-T",
                &format!("git@{host}"),
            ])
            .stdin(Stdio::null())
            .output();
        let welcomed = ssh.as_ref().is_ok_and(|o| {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.to_lowercase().contains("welcome")
        });
        checks.push(check(
            "gitlab_ssh",
            welcomed,
            format!("ssh git@{host}"),
            "load an ssh key that GitLab accepts into the agent",
        ));
    }
    checks
}
