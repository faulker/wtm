//! Thin wrapper around the `git` binary.
//!
//! All worktree, branch, status, and diff operations shell out to git rather
//! than using libgit2 bindings, because git's worktree support is most
//! complete and reliable in the CLI itself.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {args} failed: {stderr}")]
    Command { args: String, stderr: String },
    #[error("not inside a git repository")]
    NotARepo,
}

pub type Result<T> = std::result::Result<T, GitError>;

/// A single worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Branch name without the `refs/heads/` prefix; `None` when detached or bare.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_bare: bool,
    pub is_locked: bool,
    pub is_prunable: bool,
}

/// Ahead/behind counts relative to the branch's upstream, when one exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

/// One changed file from `git status --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusEntry {
    /// Two-character porcelain status code, e.g. " M", "??", "A ".
    pub code: String,
    pub path: String,
}

/// Runs git with `args` in `dir` and returns trimmed stdout, or a GitError
/// carrying stderr on non-zero exit.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    } else {
        Err(GitError::Command {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Returns the repository root (main worktree) containing `dir`.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    // `--show-toplevel` gives the current worktree's root; resolve the main
    // worktree via the common dir so config lookup works from any worktree.
    let common = run(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|_| GitError::NotARepo)?;
    let common_path = PathBuf::from(common);
    // The common dir is `<main>/.git` for normal repos; its parent is the main worktree.
    match common_path.parent() {
        Some(parent) if common_path.ends_with(".git") => Ok(parent.to_path_buf()),
        _ => Ok(common_path),
    }
}

/// Lists all worktrees of the repository containing `dir`.
pub fn list_worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let out = run(dir, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&out))
}

/// Parses `git worktree list --porcelain` output. Entries are separated by
/// blank lines; each starts with a `worktree <path>` line.
pub fn parse_worktree_porcelain(out: &str) -> Vec<Worktree> {
    let mut result = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(wt) = current.take() {
                result.push(wt);
            }
            current = Some(Worktree {
                path: PathBuf::from(path),
                branch: None,
                head: None,
                is_bare: false,
                is_locked: false,
                is_prunable: false,
            });
        } else if let Some(wt) = current.as_mut() {
            if let Some(branch) = line.strip_prefix("branch ") {
                wt.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                wt.head = Some(head.to_string());
            } else if line == "bare" {
                wt.is_bare = true;
            } else if line == "locked" || line.starts_with("locked ") {
                wt.is_locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                wt.is_prunable = true;
            }
        }
    }
    if let Some(wt) = current {
        result.push(wt);
    }
    result
}

/// True if `branch` exists as a local branch.
pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    run(
        dir,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Adds a worktree at `path` for `branch`. When `create_from` is Some, a new
/// branch is created starting at that ref; otherwise the existing branch is
/// checked out.
pub fn worktree_add(
    dir: &Path,
    path: &Path,
    branch: &str,
    create_from: Option<&str>,
) -> Result<()> {
    let path_str = path.to_string_lossy();
    match create_from {
        Some(base) => run(dir, &["worktree", "add", "-b", branch, &path_str, base])?,
        None => run(dir, &["worktree", "add", &path_str, branch])?,
    };
    Ok(())
}

/// Removes the worktree at `path`. `force` discards uncommitted changes.
pub fn worktree_remove(dir: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    run(dir, &args)?;
    Ok(())
}

/// Deletes local branch `branch` (`-D`, i.e. even if unmerged).
pub fn branch_delete(dir: &Path, branch: &str) -> Result<()> {
    run(dir, &["branch", "-D", branch])?;
    Ok(())
}

/// Changed files in the worktree at `dir` (staged, unstaged, and untracked).
pub fn status(dir: &Path) -> Result<Vec<StatusEntry>> {
    let out = run(dir, &["status", "--porcelain"])?;
    Ok(parse_status_porcelain(&out))
}

/// Parses `git status --porcelain` output.
pub fn parse_status_porcelain(out: &str) -> Vec<StatusEntry> {
    out.lines()
        .filter(|l| l.len() > 3)
        .map(|l| StatusEntry {
            code: l[..2].to_string(),
            path: l[3..].to_string(),
        })
        .collect()
}

/// Ahead/behind counts vs upstream; `None` when the branch has no upstream.
pub fn ahead_behind(dir: &Path) -> Result<Option<AheadBehind>> {
    match run(
        dir,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        Ok(out) => {
            let mut parts = out.split_whitespace();
            let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            Ok(Some(AheadBehind { ahead, behind }))
        }
        // No upstream configured is expected for fresh branches, not an error.
        Err(GitError::Command { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Unified diff of all uncommitted changes (staged + unstaged) in `dir`.
pub fn diff(dir: &Path) -> Result<String> {
    run(dir, &["diff", "HEAD"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_porcelain_multiple_entries() {
        let out = "worktree /repo\n\
                   HEAD abc123\n\
                   branch refs/heads/main\n\
                   \n\
                   worktree /repo-worktrees/feature-x\n\
                   HEAD def456\n\
                   branch refs/heads/feature-x\n\
                   \n\
                   worktree /repo-worktrees/detached\n\
                   HEAD 789abc\n\
                   detached\n";
        let wts = parse_worktree_porcelain(out);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].path, PathBuf::from("/repo"));
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[1].branch.as_deref(), Some("feature-x"));
        assert_eq!(wts[2].branch, None);
        assert_eq!(wts[2].head.as_deref(), Some("789abc"));
    }

    #[test]
    fn parses_worktree_porcelain_bare_and_locked() {
        let out = "worktree /repo\n\
                   bare\n\
                   \n\
                   worktree /wt\n\
                   HEAD abc\n\
                   branch refs/heads/x\n\
                   locked reason here\n\
                   prunable gitdir file points to non-existent location\n";
        let wts = parse_worktree_porcelain(out);
        assert!(wts[0].is_bare);
        assert!(wts[1].is_locked);
        assert!(wts[1].is_prunable);
    }

    #[test]
    fn parses_status_porcelain() {
        let out = " M src/main.rs\n?? new.txt\nA  staged.rs\n";
        let entries = parse_status_porcelain(out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].code, " M");
        assert_eq!(entries[0].path, "src/main.rs");
        assert_eq!(entries[1].code, "??");
        assert_eq!(entries[2].path, "staged.rs");
    }

    #[test]
    fn parses_empty_output() {
        assert!(parse_worktree_porcelain("").is_empty());
        assert!(parse_status_porcelain("").is_empty());
    }
}
