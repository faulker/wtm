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
    /// Stage and commit changes in a worktree.
    Commit {
        /// Worktree name.
        name: String,
        /// Commit message.
        #[arg(long, short)]
        message: String,
        /// Only stage these paths (comma-separated); default stages everything.
        #[arg(long, value_delimiter = ',')]
        paths: Option<Vec<String>>,
    },
    /// Manage a worktree's stashes (push, list, pop, apply, drop).
    Stash {
        #[command(subcommand)]
        action: StashAction,
    },
    /// Pull the latest changes for a worktree (fast-forward only by default).
    Pull {
        /// Worktree name.
        name: String,
        /// Rebase local commits onto the upstream instead of fast-forwarding.
        #[arg(long)]
        rebase: bool,
    },
    /// Push a worktree's branch (publishes to origin with -u if no upstream).
    Push {
        /// Worktree name.
        name: String,
        /// Force-push, but only if the remote hasn't moved unexpectedly.
        #[arg(long)]
        force_with_lease: bool,
    },
    /// Fetch all remotes and prune deleted remote branches.
    Fetch,
    /// Switch a worktree to check out a different branch, optionally creating it.
    Switch {
        /// Worktree name.
        name: String,
        /// Branch to check out: a local branch, or a remote-only branch (by
        /// short name or as `<remote>/<branch>`), which is checked out as a new
        /// local branch tracking the remote. With --create, a brand-new local
        /// branch of this name off the worktree's HEAD when it doesn't exist yet.
        branch: String,
        /// Create the branch off the worktree's current HEAD if it doesn't
        /// already exist anywhere (like `git switch -c`).
        #[arg(long, short)]
        create: bool,
    },
    /// Rename a worktree: renames its branch and moves its directory to match.
    Rename {
        /// Current worktree name.
        name: String,
        /// New name for the worktree (and its branch).
        new_name: String,
    },
    /// Manage branches across the repo (list, create, delete, rename).
    Branch {
        #[command(subcommand)]
        action: BranchAction,
    },
    /// Show recent commits for a worktree.
    Log {
        /// Worktree name.
        name: String,
        /// Number of commits to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        count: u32,
    },
    /// Merge a local branch into a worktree, or continue/abort a merge that
    /// stopped on conflicts.
    Merge {
        /// Branch to merge in (omit with --continue/--abort).
        source: Option<String>,
        /// Worktree to merge into.
        #[arg(long)]
        into: String,
        /// Force a merge commit even when a fast-forward would do.
        #[arg(long)]
        no_ff: bool,
        /// Finish an in-progress merge once every conflict is resolved.
        #[arg(long)]
        r#continue: bool,
        /// Abandon an in-progress merge, restoring the pre-merge state.
        #[arg(long)]
        abort: bool,
        /// Commit message for --continue (defaults to git's prepared merge message).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Merge the repository's default branch into a worktree, bringing it up
    /// to date with the mainline.
    Update {
        /// Worktree name.
        name: String,
        /// Stash uncommitted changes before the merge and reapply them after,
        /// so a dirty worktree can be updated without committing first.
        #[arg(long)]
        autostash: bool,
    },
    /// List conflicted files in a worktree mid-merge, or show one file's
    /// parsed conflict hunks.
    Conflicts {
        /// Worktree name.
        name: String,
        /// Show this file's conflict hunks instead of just listing files.
        file: Option<String>,
    },
    /// Resolve a conflicted file in a worktree mid-merge.
    Resolve {
        /// Worktree name.
        name: String,
        /// Conflicted file path, relative to the worktree root.
        file: String,
        /// Keep "our" side of every hunk in the file.
        #[arg(long)]
        ours: bool,
        /// Keep "their" side of every hunk in the file.
        #[arg(long)]
        theirs: bool,
        /// Keep both sides of every hunk, ours then theirs.
        #[arg(long)]
        both: bool,
        /// Keep both sides of every hunk, theirs then ours.
        #[arg(long)]
        both_reversed: bool,
    },
    /// Cherry-pick one or more commits from any branch into a worktree.
    CherryPick {
        /// Worktree to apply the commits into.
        #[arg(long)]
        into: String,
        /// Commits to apply, oldest-first (the order git applies them).
        #[arg(required = true)]
        commits: Vec<String>,
        /// Load the changes into the working tree without committing (git -n).
        #[arg(long)]
        no_commit: bool,
    },
    /// Set up .wtm.toml for this repo with a few guided questions (where
    /// worktrees go, files to copy, commands to run).
    Init {
        /// Replace an existing .wtm.toml.
        #[arg(long)]
        force: bool,
    },
    /// View or change settings without editing TOML by hand.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Run an MCP server over stdio exposing worktree operations as tools.
    Mcp,
}

#[derive(Debug, Subcommand)]
pub enum StashAction {
    /// Stash changes (including untracked files) in a worktree.
    Push {
        /// Worktree name.
        name: String,
        /// Optional stash message.
        #[arg(long, short)]
        message: Option<String>,
    },
    /// List a worktree's stash entries.
    List {
        /// Worktree name.
        name: String,
    },
    /// Apply and drop a stash entry (default: most recent).
    Pop {
        /// Worktree name.
        name: String,
        /// Stash entry index (default 0, the most recent).
        #[arg(long)]
        index: Option<u32>,
    },
    /// Apply a stash entry without dropping it (default: most recent).
    Apply {
        /// Worktree name.
        name: String,
        /// Stash entry index (default 0, the most recent).
        #[arg(long)]
        index: Option<u32>,
    },
    /// Drop a stash entry (default: most recent).
    Drop {
        /// Worktree name.
        name: String,
        /// Stash entry index (default 0, the most recent).
        #[arg(long)]
        index: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub enum BranchAction {
    /// List local branches with checkout, tracking, and last-commit info.
    List,
    /// Create a branch without a worktree.
    Create {
        /// Branch name.
        name: String,
        /// Base ref (defaults to HEAD).
        #[arg(long)]
        from: Option<String>,
    },
    /// Delete a branch (refuses if it's checked out in a worktree).
    Delete {
        /// Branch name.
        name: String,
        /// Delete even if unmerged (uses -D).
        #[arg(long, short)]
        force: bool,
    },
    /// Rename a branch.
    Rename {
        /// Current branch name.
        old: String,
        /// New branch name.
        new: String,
    },
    /// Show a branch's commit history (without checking it out).
    Log {
        /// Branch name.
        name: String,
        /// Number of commits to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        count: u32,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Show every setting, its value, and where it came from.
    Show,
    /// Print one setting's value (worktree_dir, setup.copy, setup.run).
    Get {
        /// Setting name.
        key: String,
    },
    /// Change a setting. worktree_dir takes "sibling", "inside", "home", or a
    /// path; setup.copy and setup.run take comma-separated lists.
    Set {
        /// Setting name.
        key: String,
        /// New value.
        value: String,
        /// Write to the global config used by all repos instead of this
        /// repo's .wtm.toml.
        #[arg(long, short)]
        global: bool,
    },
    /// Remove a setting so the default (or global value) applies again.
    Unset {
        /// Setting name.
        key: String,
        /// Remove from the global config instead of this repo's .wtm.toml.
        #[arg(long, short)]
        global: bool,
    },
    /// Print the locations of the config files wtm reads.
    Path,
}
