# wtm — worktree manager

A friendly top-level interface for git worktrees, built for working with AI agents on multiple branches at once. Create, list, inspect, and remove worktrees without knowing git commands, with automated per-repo setup (copying `.env` files, running `npm install`, and so on).

Three ways to use it:

- **TUI**: run `wtm` with no arguments inside a repo
- **CLI**: scriptable subcommands, all with `--json` output for agents
- **MCP**: `wtm mcp` serves worktree operations as MCP tools over stdio

## Setup

Requires Rust (edition 2024 toolchain) and `git` on your PATH.

```sh
cargo build --release
# then put it on your PATH, e.g.:
cp target/release/wtm ~/.local/bin/
```

## Configuration: `.wtm.toml`

Optional, lives in the repo root (main worktree). Controls where worktrees go and what happens after one is created:

```toml
# Where new worktrees are created, relative to the repo root.
# Default: ../<repo-name>-worktrees
worktree_dir = "../my-worktrees"

[setup]
# Files copied from the main worktree into the new one (if they exist).
copy = [".env", ".env.local"]
# Commands run inside the new worktree, in order. Stops at the first failure.
run = ["npm install"]
```

If a setup command fails, the worktree is kept so you can fix things by hand; `wtm create` reports the failure and exits with code 2.

## CLI

```sh
wtm create <branch> [--from <base>]   # new worktree; creates the branch if needed, runs setup
wtm list                              # all worktrees with dirty count and ahead/behind
wtm remove <name> [--force] [--delete-branch]
wtm status <name>                     # changed files in a worktree
wtm diff <name>                       # unified diff of uncommitted changes
wtm path <name>                       # prints the path, e.g. cd $(wtm path feature-x)
wtm mcp                               # MCP server over stdio
```

Worktrees are addressed by branch name (or directory name when detached). Every command accepts `--json` for machine-readable output, so agents can simply run e.g. `wtm list --json`. Errors go to stderr as `{"error": "..."}` with a non-zero exit code.

## TUI

Run `wtm` inside a repo:

| Key | Action |
| --- | --- |
| `↑`/`↓` or `j`/`k` | select worktree |
| `Enter` | view diff of uncommitted changes |
| `n` | create a new worktree (setup runs in the background with live progress) |
| `d` | delete the selected worktree (asks for confirmation; `f` to force when dirty) |
| `r` | refresh |
| `?` | help |
| `q` | quit |

## MCP server

`wtm mcp` speaks MCP over stdio and exposes `list_worktrees`, `create_worktree`, `remove_worktree`, `worktree_status`, and `worktree_diff`. Results use the same JSON shapes as the CLI's `--json` output.

Register with [Claude Code](https://claude.com/claude-code) from inside your repo:

```sh
claude mcp add wtm -- wtm mcp
```

The server binds to the repo it was started in and reloads `.wtm.toml` on every call.

## Build and test

```sh
cargo build            # debug build
cargo test             # unit + integration tests (temp git repos, MCP stdio session)
cargo build --release  # optimized binary at target/release/wtm
```

## Project layout

```
src/git.rs      thin wrapper around the git binary (worktree/status/diff parsing)
src/config.rs   .wtm.toml loading
src/ops.rs      core operations shared by CLI, TUI, and MCP
src/cli.rs      clap definitions
src/output.rs   human vs JSON rendering
src/tui/        ratatui app (state, rendering, event loop)
src/mcp.rs      MCP stdio server (rmcp)
tests/          end-to-end tests against throwaway git repos
```
