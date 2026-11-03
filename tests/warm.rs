mod common;

use common::{Fixture, REPO, TestResult};
use filetime::FileTime;
use ws::cmd::create::CreateReport;
use ws::cmd::create::spike;
use ws::warm::{WarmOutcome, Warmth};

fn target(report: &CreateReport) -> Option<&WarmOutcome> {
    report.worktrees[0]
        .warm
        .iter()
        .find(|w| w.dir.as_str() == "target")
        .map(|w| &w.outcome)
}

// Cargo decides freshness by mtimes, and node_modules relies on symlinks; a
// warm copy that loses either is a cold build in disguise.
#[test]
fn copies_build_dirs_with_mtimes_and_links_once() -> TestResult {
    let f = Fixture::new()?;
    let repos = vec![REPO.parse()?];
    spike(&f.ws()?, "a".parse()?, &repos, Warmth::Cold)?;
    let built = f.worktree("a").join("target");
    std::fs::create_dir_all(built.join("debug/deps"))?;
    let artifact = built.join("debug/deps/libx.rlib");
    std::fs::write(&artifact, "artifact")?;
    let old = FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(&artifact, old)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("deps/libx.rlib", built.join("debug/link"))?;

    let report = spike(&f.ws()?, "b".parse()?, &repos, Warmth::Warm)?;
    assert!(matches!(
        target(&report),
        Some(WarmOutcome::Copied { files: 1, .. })
    ));

    let copy = f.worktree("b").join("target/debug/deps/libx.rlib");
    assert_eq!(std::fs::read_to_string(&copy)?, "artifact");
    assert_eq!(
        FileTime::from_last_modification_time(&std::fs::metadata(&copy)?),
        old
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(f.worktree("b").join("target/debug/link"))?,
        std::path::Path::new("deps/libx.rlib")
    );
    assert!(!f.worktree("b").join("target.ws-warm").exists());

    let again = spike(&f.ws()?, "b".parse()?, &repos, Warmth::Warm)?;
    // An existing worktree is in use: a re-run does not copy under it.
    assert!(again.worktrees[0].warm.is_empty());
    Ok(())
}
