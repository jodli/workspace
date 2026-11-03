mod common;

use common::{Fixture, REPO, TestResult, commit, git};
use ws::branch::{ChangeType, IssueType};
use ws::cmd::create::{CreateOutcome, spike, start};
use ws::cmd::drop::{DropReport, drop};
use ws::removal::{MergedProof, Safety};
use ws::warm::Warmth;
use ws::worktree::{Basis, Keep};

fn repos() -> TestResult<Vec<ws::names::RepoName>> {
    Ok(vec![REPO.parse()?])
}

fn drop_with(f: &Fixture, space: &str, merged: &[String]) -> TestResult<DropReport> {
    let proofs = merged
        .iter()
        .map(|m| m.parse::<MergedProof>())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(drop(&f.ws()?, &space.parse()?, &proofs, Safety::Checked)?)
}

#[test]
fn keeps_uncommitted_and_unpushed_work() -> TestResult {
    let f = Fixture::new()?;
    spike(&f.ws()?, "s".parse()?, &repos()?, Warmth::Cold)?;
    let wt = f.worktree("s");

    std::fs::write(wt.join("scratch.txt"), "wip")?;
    let r = drop_with(&f, "s", &[])?;
    assert!(matches!(r.removal.kept[0].keep, Keep::Uncommitted { .. }));
    assert!(!r.removal.space_removed && wt.exists());
    std::fs::remove_file(wt.join("scratch.txt"))?;

    commit(&wt, "work.txt", "local only")?;
    let r = drop_with(&f, "s", &[])?;
    assert!(matches!(
        r.removal.kept[0].keep,
        Keep::Unpushed { commits: 1 }
    ));
    assert!(wt.exists());
    Ok(())
}

#[test]
fn clean_fresh_space_goes_with_branch_and_notes_archived() -> TestResult {
    let f = Fixture::new()?;
    spike(&f.ws()?, "s".parse()?, &repos()?, Warmth::Cold)?;
    let r = drop_with(&f, "s", &[])?;
    assert!(r.removal.space_removed);
    assert!(matches!(r.removal.removed[0].basis, Basis::Local));
    assert!(r.removal.removed[0].branch_deleted);
    assert!(f.root.join("spaces/.archive/s.md").is_file());
    assert_eq!(
        git(
            &f.root.join("repos").join(REPO),
            &["branch", "--list", "spike/s"]
        )?,
        ""
    );
    Ok(())
}

#[test]
fn pushed_but_unmerged_branch_survives_its_worktree() -> TestResult {
    let f = Fixture::new()?;
    spike(&f.ws()?, "s".parse()?, &repos()?, Warmth::Cold)?;
    let wt = f.worktree("s");
    commit(&wt, "work.txt", "pushed")?;
    git(&wt, &["push", "--quiet", "-u", "origin", "spike/s"])?;

    let r = drop_with(&f, "s", &[])?;
    assert!(matches!(r.removal.removed[0].basis, Basis::Local));
    assert!(!r.removal.removed[0].branch_deleted);
    Ok(())
}

// The squash case: commits never reach main, the remote branch is deleted.
// Only the agent's proof from GitLab lets the worktree go, and only for the
// exact head it names.
#[test]
fn squash_merge_needs_proof_of_the_exact_head() -> TestResult {
    let f = Fixture::new()?;
    spike(&f.ws()?, "s".parse()?, &repos()?, Warmth::Cold)?;
    let wt = f.worktree("s");
    let head = commit(&wt, "work.txt", "feature")?;
    git(&wt, &["push", "--quiet", "-u", "origin", "spike/s"])?;
    git(&f.origin, &["branch", "-D", "spike/s"])?;

    let r = drop_with(&f, "s", &[])?;
    assert!(matches!(r.removal.kept[0].keep, Keep::Unpushed { .. }));
    assert!(
        r.removal.kept[0]
            .mr_check
            .as_ref()
            .is_some_and(|c| c.if_merged == format!("--merged {REPO}={head}"))
    );

    let stale = format!("{REPO}={}", "0".repeat(40));
    let r = drop_with(&f, "s", &[stale])?;
    assert!(matches!(r.removal.kept[0].keep, Keep::HeadMoved { .. }));

    let r = drop_with(&f, "s", &[format!("{REPO}={head}")])?;
    assert!(r.removal.space_removed);
    assert!(matches!(r.removal.removed[0].basis, Basis::Merged { .. }));
    Ok(())
}

#[test]
fn start_is_idempotent_and_refuses_a_different_purpose() -> TestResult {
    let f = Fixture::new()?;
    let key: ws::names::JiraKey = "DEMO-1".parse()?;
    let first = start(
        &f.ws()?,
        key.clone(),
        IssueType::Story,
        None,
        &repos()?,
        Warmth::Cold,
    )?;
    assert_eq!(first.worktrees[0].branch.as_str(), "feat/DEMO-1");
    assert!(matches!(
        first.worktrees[0].outcome,
        CreateOutcome::Created { from_origin: true }
    ));

    let again = start(
        &f.ws()?,
        key.clone(),
        IssueType::Story,
        None,
        &repos()?,
        Warmth::Cold,
    )?;
    assert!(matches!(
        again.worktrees[0].outcome,
        CreateOutcome::Existing { .. }
    ));
    assert!(!again.notes_created);

    let other = start(
        &f.ws()?,
        key,
        IssueType::Task,
        Some(ChangeType::Chore),
        &repos()?,
        Warmth::Cold,
    );
    assert!(other.is_err_and(|e| e.report().kind == "input.space_conflict"));
    Ok(())
}

// prune --yes must leave work in progress alone even though drop would accept
// it, and clear a branch once it landed on main.
#[test]
fn prune_only_clears_landed_work() -> TestResult {
    use ws::cmd::prune::{Apply, WorktreeVerdict, prune};
    let f = Fixture::new()?;
    spike(&f.ws()?, "open".parse()?, &repos()?, Warmth::Cold)?;
    spike(&f.ws()?, "landed".parse()?, &repos()?, Warmth::Cold)?;
    for space in ["open", "landed"] {
        let wt = f.worktree(space);
        commit(&wt, &format!("{space}.txt"), space)?;
        git(
            &wt,
            &["push", "--quiet", "-u", "origin", &format!("spike/{space}")],
        )?;
    }
    // Fast-forward merge of `landed` on the server, source branch deleted.
    git(
        &f.origin,
        &["update-ref", "refs/heads/main", "refs/heads/spike/landed"],
    )?;
    git(&f.origin, &["branch", "-D", "spike/landed"])?;

    let r = prune(&f.ws()?, &[], Apply::Remove)?;
    let verdict = |name: &str| r.spaces.iter().find(|s| s.space.as_str() == name);
    assert!(
        verdict("open").is_some_and(
            |s| !s.finished && matches!(s.worktrees[0], WorktreeVerdict::Active { .. })
        )
    );
    assert!(verdict("landed").is_some_and(|s| s.finished));
    assert!(f.worktree("open").exists());
    assert!(!f.worktree("landed").exists());
    Ok(())
}
