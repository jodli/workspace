//! `ws doctor`: everything that has to hold before the other commands work.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::Serialize;

use super::Report;
use crate::error::{Error, Exit};
use crate::git::Git;
use crate::workspace::{Canonical, Workspace, par_map};

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
        Exit::incomplete_if(self.checks.iter().any(|c| !c.ok))
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

/// Passes when nothing offends; the detail lists the offenders otherwise.
fn none_of(
    name: &'static str,
    offenders: &[String],
    ok_detail: String,
    hint: &'static str,
) -> Check {
    let detail = if offenders.is_empty() {
        ok_detail
    } else {
        offenders.join(", ")
    };
    check(name, offenders.is_empty(), detail, hint)
}

pub fn doctor(ws: &Workspace) -> Result<DoctorReport, Error> {
    // The network checks can take seconds; the local ones run meanwhile.
    let (network, local) = std::thread::scope(|s| {
        let net = s.spawn(|| network(ws));
        let local = local(ws);
        (
            net.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
            local,
        )
    });
    let mut checks = local?;
    checks.extend(network);
    Ok(DoctorReport { checks })
}

fn local(ws: &Workspace) -> Result<Vec<Check>, Error> {
    let mut checks = vec![Git::version().map_or_else(
        |e| {
            check(
                "git",
                false,
                e.to_string(),
                "install git and put it on PATH",
            )
        },
        |v| check("git", true, v, ""),
    )];

    let missing: Vec<String> = ws
        .roster
        .iter()
        .filter(|e| ws.repo(e).is_none())
        .map(|e| e.name.to_string())
        .collect();
    checks.push(none_of(
        "repos_cloned",
        &missing,
        format!("{} repos", ws.roster.iter().count()),
        "`ws sync` clones them",
    ));

    let repos: Vec<_> = ws.repos().collect();
    let off: Vec<String> = par_map(&repos, |r| {
        r.facts().map(|f| (r.entry.name.to_string(), f.canonical))
    })
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .filter(|(_, c)| *c != Canonical::Ok)
    .map(|(name, c)| format!("{name}: {c:?}"))
    .collect();
    checks.push(none_of(
        "repos_canonical",
        &off,
        "all on a clean default branch".to_owned(),
        "move work into a space with `ws adopt`; that also restores the repo",
    ));

    let legacy = ws.root.join("spikes");
    checks.push(check(
        "no_legacy_spikes_dir",
        !legacy.exists(),
        legacy.display().to_string(),
        "move each worktree into spaces/ (see the ws skill), then remove spikes/",
    ));
    Ok(checks)
}

/// Reachability first: without it the ssh check would only time out.
fn network(ws: &Workspace) -> Vec<Check> {
    let host = ws.config.gitlab.host.as_str();
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
    // Through git, so it tests the ssh setup git actually uses.
    if reachable && let Some(entry) = ws.roster.iter().next() {
        let url = ws.config.gitlab.ssh_url(&entry.gitlab);
        checks.push(match Git::ls_remote(&url) {
            Ok(()) => check("gitlab_ssh", true, url, ""),
            Err(e) => check(
                "gitlab_ssh",
                false,
                e.to_string(),
                "load an ssh key that GitLab accepts into the agent",
            ),
        });
    }
    checks
}
