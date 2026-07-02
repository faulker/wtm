//! Command-line interface definitions (clap).

use clap::{Parser, Subcommand};

/// wtm — a friendly manager for git worktrees.
///
/// Run without a subcommand to open the interactive TUI.
#[derive(Debug, Parser)]
#[command(name = "wtm", version, about)]
pub struct Cli {
    /// Output machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a worktree for a branch (creating the branch if needed) and run
    /// the setup steps from .wtm.toml.
    Create {
        /// Branch to check out in the new worktree.
        branch: String,
        /// Base ref for a newly created branch (defaults to HEAD).
        #[arg(long)]
        from: Option<String>,
    },
    /// List all worktrees with branch, path, and change status.
    List,
    /// Remove a worktree (refuses if it has uncommitted changes).
    Remove {
        /// Worktree name (branch name, or directory name when detached).
        name: String,
        /// Discard uncommitted changes.
        #[arg(long, short)]
        force: bool,
        /// Also delete the worktree's local branch.
        #[arg(long)]
        delete_branch: bool,
    },
    /// Show changed files in a worktree.
    Status {
        /// Worktree name.
        name: String,
    },
    /// Show the diff of uncommitted changes in a worktree.
    Diff {
        /// Worktree name.
        name: String,
    },
    /// Print a worktree's absolute path (handy for `cd $(wtm path foo)`).
    Path {
        /// Worktree name.
        name: String,
    },
    /// Run an MCP server over stdio exposing worktree operations as tools.
    Mcp,
}
