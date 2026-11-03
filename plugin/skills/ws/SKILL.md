---
name: ws
description: Multi-repo worktree workspace. Use for any work inside a ws workspace (a directory with ws.toml): starting work on a Jira ticket or a spike, finding where work lives, reviewing an existing branch, cleaning up finished work. repos/ is read-only; all work happens in spaces/.
---

# ws

```text
repos/<repo>            canonical checkout on the default branch. Read only.
spaces/<space>/<repo>   a worktree per repo; work happens here
spaces/<space>/SPACE.md notes for the space, below the +++ frontmatter
```

Every command prints one JSON object. Exit 0: done. Exit 1: done in part, the
result says what was kept or failed. Exit 2 to 6: `{"error": {kind, message, hint}}`;
the hint names the fix.

## Start work

1. Read the issue type in Jira and map it:
   - Bug: `--issue bug`
   - Story: `--issue story`
   - Task: `--issue task --change <c>`. Judge `<c>` from the ticket: `feat`, `fix`, `chore`, `refactor`, `docs`, `test`, `ci`, `perf` or `build`.
   - Sub-task: use the parent ticket's type with the rule above.
   - Epic: not worked on directly. Start one of its child tickets.
2. `ws start <KEY> --issue <type> [--change <c>] <repo>...`. Work in the
   `path` of each worktree in the result. The branch prefix follows each
   repo's convention.
3. Exploration without a ticket: `ws spike <name> <repo>...`.
4. An existing branch, e.g. to review an MR: `ws adopt <space> <repo> <branch>`.

Re-running any of these is safe. New worktrees get `target/` and
`node_modules/` copied from another worktree; pass `--cold` to skip that.

Follow the repo's commit convention. Where release tooling parses
conventional commits, the commit type sets the version, not the branch name.

## Look

`ws list` shows every repo and space. `ws status <space>` shows one.

## Clean up

1. `ws prune` reports which spaces are finished. It changes nothing.
2. For entries with `mr_check`, look up the MR:
   `glab api "projects/<project, url-encoded>/merge_requests?source_branch=<branch>&state=merged"`.
   If a merged MR's `sha` equals the worktree's `head`, pass `mr_check.if_merged`.
3. `ws prune --yes [--merged <repo>=<sha>...]` removes the finished spaces.
4. `ws drop <space> [--merged ...]` removes one space now, as far as nothing
   unpushed would be lost. `--force` loses work: only when the user asks.

## Repos

- `ws sync`: fetch and fast-forward every default branch.
- `ws add <gitlab path> [--gitflow]`: add a repo. Find paths with
  `glab api "projects?search=<term>&membership=true"`. `--gitflow` for repos
  that name branches `feature/` and `bugfix/`.
- `ws doctor`: git, GitLab reachability, workspace invariants.

The plugin's hook denies writes into `repos/` and raw `git worktree`
commands; the denial names the `ws` command to use.
