# ws

A multi-repo worktree workspace, driven by coding agents. One space per
ticket or spike, with a git worktree per repo it touches, while the
canonical clones stay untouched on their default branch.

```text
<root>/ws.toml                config, marks the root (see ws.example.toml)
<root>/repos.tsv              roster: <name><TAB><remote path>[<TAB>gitflow]
<root>/repos/<repo>           canonical clone, default branch, read only
<root>/spaces/<space>/<repo>  worktrees; SPACE.md holds the notes
```

Every command prints JSON for an agent to read; `ws --help` lists them. The
workflow an agent follows is in [plugin/skills/ws/SKILL.md](plugin/skills/ws/SKILL.md).

## Install

```bash
cargo install --git https://github.com/jodli/workspace
```

Then the Claude Code plugin, which brings the skill and the guard hook (the
hook runs `ws guard`, so `ws` must be on the PATH Claude Code sees):

```text
/plugin marketplace add jodli/workspace
/plugin install ws@workspace
```

## Warm build dirs

New worktrees get `target/` and `node_modules/` from the most recently built
worktree of the same repo. On APFS (and btrfs/XFS) that is a copy-on-write
clone: instant, no extra disk. On NTFS it is a full copy, or skipped with
`warm.fallback = "skip"`.
