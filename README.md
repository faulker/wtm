# wtm — worktree manager

A friendly top-level interface for git, built for working with AI agents on multiple branches at once. Create, list, inspect, and remove worktrees without knowing git commands, with automated per-repo setup (copying `.env` files, running `npm install`, and so on). Beyond worktrees it covers the everyday git workflow too: commit, stash, pull, push, fetch, branches, and log, all addressed by worktree name instead of paths and flags.

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

## Settings

Every repo must be initialized before worktree commands work: until a `.wtm.toml` exists in the repo root, `create`, `list`, and friends refuse with a pointer to `wtm init` (MCP tool calls report the same error). There are two ways to initialize:

- **`wtm init`**: a guided wizard in the terminal. It first offers to clone settings from another repo (give a path to the repo or its `.wtm.toml`), otherwise it asks where worktrees should go and what setup each new one needs, then writes `.wtm.toml`.
- **run `wtm` with no arguments**: in an uninitialized repo the TUI opens straight into a setup wizard. It starts with the same clone question; press `Tab` on the path prompt to pick the source with a file browser instead of typing. Both routes end on a review screen where you can still edit every setting before the file is written.

```sh
wtm init
```

To view or change settings later, no TOML editing required:

```sh
wtm config                       # show every setting, its value, and where it came from
wtm config get worktree_dir
wtm config set worktree_dir inside
wtm config set open_command "cursor ."
wtm config set setup.copy ".env, .env.local"
wtm config unset setup.copy      # back to the default (or the global value)
wtm config path                  # where the config files live
```

### Where worktrees go: `worktree_dir`

Pick a predefined rule or give a path yourself:

| Value | Worktrees end up in |
| --- | --- |
| `sibling` (default) | `../<repo>-worktrees`, next to the repo |
| `inside` | `.worktrees/` inside the repo (kept out of `git status` automatically) |
| `home` | `~/worktrees/<repo>` |
| any path | absolute, `~/...`, or relative to the repo root; `{repo}` expands to the repo folder name, e.g. `~/wt/{repo}` |

### Two config layers

Settings resolve per field: repo, then global, then built-in default.

- **Repo**: `.wtm.toml` in the repo root, applies to this repo only.
- **Global**: `~/.config/wtm/config.toml` (or `$XDG_CONFIG_HOME/wtm/config.toml`), applies to all your repos. Write to it with `wtm config set --global <key> <value>`.

`wtm config` shows which layer each value came from.

### The config file

`wtm init` and `wtm config set` maintain this for you (comments are preserved), but it's plain TOML if you'd rather edit by hand:

```toml
# "sibling", "inside", "home", or a path ({repo} = repo folder name)
worktree_dir = "sibling"
# Command the TUI's `e` key runs in a worktree's directory (e.g. open an editor).
open_command = "cursor ."

[setup]
# Files copied from the main worktree into the new one (if they exist).
# Files in subfolders (e.g. "config/.env") land in the same subfolder.
copy = [".env", ".env.local"]
# Commands run inside the new worktree, in order. Stops at the first failure.
run = ["npm install"]
```

If a setup command fails, the worktree is kept so you can fix things by hand; `wtm create` reports the failure and exits with code 2.

Setup commands are interactive: with `wtm create` in a terminal they attach to your terminal directly, and in the TUI their output streams live into the progress window, where you can type a line and press `Enter` to answer a prompt. If a command hangs, press `Ctrl+C` twice in the TUI to kill it (the worktree itself is kept).

## CLI

```sh
wtm init [--force]                    # guided setup, writes .wtm.toml
wtm create <branch> [--from <base>]   # new worktree; creates the branch if needed, runs setup
wtm list                              # all worktrees with dirty count and ahead/behind
wtm remove <name> [--force] [--delete-branch]
wtm status <name>                     # changed files in a worktree
wtm diff <name>                       # unified diff of uncommitted changes
wtm path <name>                       # prints the path, e.g. cd $(wtm path feature-x)
wtm config [show|get|set|unset|path]  # view and change settings
wtm mcp                               # MCP server over stdio
```

Everyday git, addressed by worktree name:

```sh
wtm commit <name> -m <msg> [--paths a,b]   # stage (everything, or just --paths) and commit
wtm stash push <name> [-m <msg>]           # stash changes, untracked files included
wtm stash list|pop|apply|drop <name> [--index N]
wtm pull <name> [--rebase]                 # fast-forward only unless --rebase
wtm push <name> [--force-with-lease]       # publishes with -u origin when no upstream yet
wtm fetch                                  # fetch all remotes, prune deleted branches
wtm branch list                            # branches with checkout, tracking, last commit
wtm branch create <name> [--from <ref>]    # branch without a worktree
wtm branch delete <name> [--force]         # refuses if checked out in a worktree
wtm branch rename <old> <new>
wtm log <name> [-n <count>]                # recent commits (default 20)
```

`wtm create` also pulls down remote branches: when the branch only exists on a remote, it creates a local tracking branch from it instead of branching off HEAD.

Everyday git operations, each scoped to one worktree addressed by name:

```sh
wtm commit <name> -m <msg> [--paths a,b]   # stage (all, or just these paths) and commit
wtm log <name> [-n <count>]                # recent commits (default 20)
wtm pull <name> [--rebase]                 # fast-forward pull, or rebase; errors if no upstream
wtm push <name> [--force-with-lease]       # push; publishes to origin with -u if no upstream
wtm stash push <name> [-m <msg>]           # stash changes, including untracked files
wtm stash list <name>                      # list stash entries
wtm stash pop|apply|drop <name> [--index N]
```

Repo-wide commands (not tied to a single worktree):

```sh
wtm fetch                                  # fetch all remotes and prune deleted branches
wtm branch list                            # local branches: checkout, tracking, last commit
wtm branch create <name> [--from <ref>]    # create a branch without a worktree
wtm branch delete <name> [--force]         # delete; refuses if checked out in a worktree
wtm branch rename <old> <new>
```

When `wtm create <branch>` is given a branch that only exists on a remote (e.g. `origin/<branch>`), it fetches if needed and checks out a local tracking branch from the remote instead of branching from HEAD.

Worktrees are addressed by branch name (or directory name when detached). Every command accepts `--json` for machine-readable output, so agents can simply run e.g. `wtm list --json`. Errors go to stderr as `{"error": "..."}` with a non-zero exit code.

## TUI

Run `wtm` inside a repo. If the repo isn't initialized yet, the setup wizard opens first (see [Settings](#settings)); once `.wtm.toml` exists you get the worktree list:

| Key | Action |
| --- | --- |
| `↑`/`↓` or `j`/`k` | select worktree |
| `Enter` | browse changes in a folder tree: the left panel groups changed files under their folders (a folder shows `[x]`/`[ ]`/`[~]` for all/none/some of its files marked); pick a file to see its diff on the right. `Space` marks/unmarks the file, or the whole folder when the cursor is on a folder row; `s` stashes just the highlighted file, `⇧S` stashes every marked (`[x]`) file, `⇧R` reverts the highlighted file, `c` commits the marked files, `i` adds the file or folder to `.gitignore` (choose the exact path or a glob that ignores everything like it), `?` shows help. New files inside brand-new folders are listed too, so you can view their contents. Updates live as files change; `r` refreshes now |
| `n` | new **worktree**. The top row creates a **new branch** (named as you type) branched off a base branch — press `Tab` to choose the base (defaults to the main branch). The rows below **check out an existing branch** in a worktree. To make a branch *without* a worktree, use the branch browser (`b`) instead. If the target folder already exists you're asked to open it (when it's already a worktree), replace it, or cancel |
| `d` | delete the selected worktree: choose folder-only (keeps the branch) or folder + branch; `f` to force when dirty |
| `c` | commit the selected worktree: tick which changed files to include (all selected by default; `Tab` switches between the file list and the message, `Space` toggles a file), type a message, `Enter` commits |
| `o` | options: edit this repo's settings (`worktree_dir`, `open_command`, `setup.copy`, `setup.run`) without touching the file |
| `e` | run the `open_command` in the selected worktree's directory (e.g. `cursor .`); prompts for a command when `open_command` isn't set |
| `s` | stash manager: `s` stash current changes, `p` pop, `a` apply, `x` drop the selected entry |
| `p` | pull the selected worktree (fast-forward only) |
| `⇧P` | push the selected worktree; publishes with `-u` when there's no upstream |
| `f` | fetch all remotes and refresh |
| `b` | branch browser: every local branch with where it's checked out. `n` creates a **branch only** (no worktree, from HEAD), `x` deletes, `Enter` checks the selected branch out in a new worktree |
| `l` | log of recent commits for the selected worktree |
| `r` | refresh |
| `?` | help (works here and in the changes view; any key closes it) |
| `q` / `Ctrl+C` | quit |

Text fields (like the new-branch name) support cursor editing: `←`/`→` move, `Home`/`End` jump, and `Backspace`/`Delete` remove characters mid-string.

Pressing `o` opens an editor for the repo's `.wtm.toml`: pick a row with `↑`/`↓`, press `Enter` to edit it, and select the save row to write. It shows a live preview of where worktrees will land, preserves any comments in the file, and clearing a field unsets it so the default (or global value) applies again.

While setup runs, its output streams into the progress window. Type a line and press `Enter` to answer a prompting command; press `Ctrl+C` twice to kill a stuck setup.

## MCP server

`wtm mcp` speaks MCP over stdio and exposes the same operations as the CLI. Results use the same JSON shapes as the CLI's `--json` output.

| Area | Tools |
| --- | --- |
| Worktrees | `list_worktrees`, `create_worktree`, `remove_worktree`, `worktree_status`, `worktree_diff` |
| Commits | `commit_changes`, `worktree_log` |
| Stashes | `stash_push`, `stash_list`, `stash_pop`, `stash_apply`, `stash_drop` |
| Remotes | `pull_worktree`, `push_worktree`, `fetch_remotes` |
| Branches | `list_branches`, `create_branch`, `delete_branch`, `rename_branch` |

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
src/config.rs   layered config: global file + repo .wtm.toml, location rules
src/settings.rs wtm config and wtm init commands
src/ops.rs      core operations shared by CLI, TUI, and MCP
src/cli.rs      clap definitions
src/output.rs   human vs JSON rendering
src/tui/        ratatui app (state, rendering, event loop)
src/mcp.rs      MCP stdio server (rmcp)
tests/          end-to-end tests against throwaway git repos
```
