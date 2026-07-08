# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

wtm (worktree manager) is a friendly top-level interface for git, built for running AI agents on multiple branches at once. It creates, lists, inspects, and removes git worktrees (with automated per-repo setup like copying `.env` files and running `npm install`) and wraps the everyday git workflow (commit, stash, pull, push, fetch, branches, log), all addressed by worktree name instead of paths and flags.

## Tech Stack

Rust 2024 edition. `ratatui` for the TUI, `clap` for the CLI, `rmcp` for the MCP server, `tokio` async runtime, `serde`/`serde_json` and `toml`/`toml_edit` for config, `anyhow`/`thiserror` for errors. `git` must be on `PATH`.

## Build, Run, Test

```sh
cargo build --release          # binary -> target/release/wtm
cargo test                     # unit + integration tests (tests/integration.rs)
cargo clippy
wtm                            # no args inside a repo: launch the TUI
wtm <subcommand> --json        # scriptable CLI, JSON output for agents
wtm mcp                        # serve worktree operations as MCP tools over stdio
```

Every repo must be initialized (`wtm init`, or the TUI setup wizard) before worktree commands work; until a `.wtm.toml` exists in the repo root they refuse with a pointer to `wtm init`. Settings are managed via `wtm config get/set/unset` rather than editing TOML by hand.

## Three Interfaces, One Core

The same operations are exposed three ways, so keep logic in the shared core and let each surface call into it:
- **TUI** (`src/tui/`): `app.rs` (state), `ui.rs` (rendering), `setup.rs` (init wizard), `config_editor.rs`.
- **CLI** (`src/cli.rs`, `src/main.rs`): clap subcommands, `--json` output via `src/output.rs`.
- **MCP** (`src/mcp.rs`): worktree operations as MCP tools over stdio.

Shared core:
- `src/ops.rs`: the high-level worktree operations (the largest module; the heart of the app).
- `src/git.rs`: git command execution and parsing.
- `src/config.rs` + `src/settings.rs`: `.wtm.toml` loading, defaults, and the settings model.

## Model Selection

- **Claude Fable 5** (`claude-fable-5`): git-plumbing correctness and edge cases in `git.rs` and `ops.rs`, the settings/config resolution model, and keeping the TUI, CLI, and MCP surfaces consistent with the shared core.
- **Claude Opus 4.8** (`claude-opus-4-8`): default for adding a command or operation across `ops.rs` plus the CLI, TUI, and MCP surfaces.
- **Claude Sonnet 5** (`claude-sonnet-5`): single-surface tweaks, output formatting, and tests in `tests/integration.rs`.
- **Claude Haiku 4.5** (`claude-haiku-4-5`): README/docs, help text, and quick lookups.
