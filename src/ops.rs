//! Core worktree operations shared by the CLI, TUI, and MCP server.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::config::Config;
use crate::git::{self, AheadBehind, StatusEntry};

/// Everything an operation needs to know about the repo it acts on.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Main worktree root (where `.wtm.toml` lives).
    pub repo_root: PathBuf,
    pub config: Config,
}

impl Ctx {
    /// Discovers the repo containing `cwd` and loads its config.
    pub fn discover(cwd: &Path) -> Result<Ctx> {
        let repo_root = git::repo_root(cwd)?;
        let config = Config::load(&repo_root)?;
        Ok(Ctx { repo_root, config })
    }
}

/// A worktree with the status information shown in lists. Serialized as-is
/// for `--json` output and MCP results.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    /// Short name used to address the worktree in commands (branch name, or
    /// directory name when detached).
    pub name: String,
    pub branch: Option<String>,
    pub path: String,
    pub is_main: bool,
    /// Number of changed files (staged + unstaged + untracked).
    pub dirty: usize,
    /// Ahead/behind upstream; `null` when no upstream is configured.
    pub ahead_behind: Option<AheadBehind>,
    pub locked: bool,
}

/// Outcome of one setup step during `create`.
#[derive(Debug, Clone, Serialize)]
pub struct SetupStep {
    /// e.g. `copy .env` or `run npm install`.
    pub step: String,
    pub ok: bool,
    /// Failure or skip reason when not ok.
    pub detail: Option<String>,
}

/// Result of `create`, including what setup did.
#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub name: String,
    pub branch: String,
    pub path: String,
    /// True when the branch was newly created rather than checked out.
    pub created_branch: bool,
    pub setup: Vec<SetupStep>,
    /// True when every setup step succeeded.
    pub setup_ok: bool,
}

/// Lists all worktrees with dirty counts and ahead/behind info.
pub fn list(ctx: &Ctx) -> Result<Vec<WorktreeInfo>> {
    let wts = git::list_worktrees(&ctx.repo_root)?;
    let mut infos = Vec::with_capacity(wts.len());
    for wt in wts {
        if wt.is_bare {
            continue;
        }
        let is_main = wt.path == ctx.repo_root;
        // A worktree directory can disappear out from under git (deleted by
        // hand); report it rather than failing the whole listing.
        let exists = wt.path.exists();
        let (dirty, ahead_behind) = if exists {
            (git::status(&wt.path)?.len(), git::ahead_behind(&wt.path)?)
        } else {
            (0, None)
        };
        infos.push(WorktreeInfo {
            name: worktree_name(&wt.branch, &wt.path),
            branch: wt.branch,
            path: wt.path.to_string_lossy().to_string(),
            is_main,
            dirty,
            ahead_behind,
            locked: wt.is_locked,
        });
    }
    Ok(infos)
}

/// Creates a worktree for `branch` (creating the branch from `from`/HEAD when
/// it doesn't exist), then runs the configured setup steps. `progress` is
/// called with a human-readable line before each long-running step.
pub fn create(
    ctx: &Ctx,
    branch: &str,
    from: Option<&str>,
    mut progress: impl FnMut(&str),
) -> Result<CreateResult> {
    if branch.trim().is_empty() {
        bail!("branch name must not be empty");
    }
    if let Some(existing) = find(ctx, branch)? {
        bail!(
            "branch '{branch}' is already checked out at {}",
            existing.path
        );
    }
    let base = ctx.config.worktree_base(&ctx.repo_root);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("failed to create {}", base.display()))?;
    // Canonicalize so reported paths match what git prints in worktree lists.
    let base = std::fs::canonicalize(&base)?;
    let path = base.join(sanitize_dir_name(branch));
    if path.exists() {
        bail!("target directory already exists: {}", path.display());
    }

    let create_branch = !git::branch_exists(&ctx.repo_root, branch);
    if !create_branch && from.is_some() {
        bail!("branch '{branch}' already exists; --from only applies to new branches");
    }
    progress(&format!("creating worktree at {}", path.display()));
    let create_from = create_branch.then(|| from.unwrap_or("HEAD"));
    git::worktree_add(&ctx.repo_root, &path, branch, create_from)?;

    let mut setup = Vec::new();
    for file in &ctx.config.setup.copy {
        setup.push(copy_step(&ctx.repo_root, &path, file));
    }
    for cmd in &ctx.config.setup.run {
        progress(&format!("running: {cmd}"));
        let step = run_step(&path, cmd);
        let failed = !step.ok;
        setup.push(step);
        if failed {
            // Later commands often depend on earlier ones (e.g. npm install),
            // so stop rather than cascade failures.
            for skipped in ctx
                .config
                .setup
                .run
                .iter()
                .skip_while(|c| *c != cmd)
                .skip(1)
            {
                setup.push(SetupStep {
                    step: format!("run {skipped}"),
                    ok: false,
                    detail: Some("skipped: earlier setup command failed".to_string()),
                });
            }
            break;
        }
    }

    let setup_ok = setup.iter().all(|s| s.ok);
    Ok(CreateResult {
        name: branch.to_string(),
        branch: branch.to_string(),
        path: path.to_string_lossy().to_string(),
        created_branch: create_branch,
        setup,
        setup_ok,
    })
}

/// Removes the worktree named `name`. Refuses when dirty unless `force`;
/// `delete_branch` also deletes its local branch afterwards.
pub fn remove(ctx: &Ctx, name: &str, force: bool, delete_branch: bool) -> Result<WorktreeInfo> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    if info.is_main {
        bail!("refusing to remove the main worktree");
    }
    if info.dirty > 0 && !force {
        bail!(
            "worktree '{}' has {} uncommitted change(s); use --force to discard them",
            info.name,
            info.dirty
        );
    }
    git::worktree_remove(&ctx.repo_root, Path::new(&info.path), force)?;
    if delete_branch {
        if let Some(branch) = &info.branch {
            git::branch_delete(&ctx.repo_root, branch)?;
        }
    }
    Ok(info)
}

/// Changed files for the worktree named `name`.
pub fn status(ctx: &Ctx, name: &str) -> Result<(WorktreeInfo, Vec<StatusEntry>)> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let entries = git::status(Path::new(&info.path))?;
    Ok((info, entries))
}

/// Unified diff of uncommitted changes in the worktree named `name`.
pub fn diff(ctx: &Ctx, name: &str) -> Result<(WorktreeInfo, String)> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let diff = git::diff(Path::new(&info.path))?;
    Ok((info, diff))
}

/// Absolute path of the worktree named `name`.
pub fn path(ctx: &Ctx, name: &str) -> Result<String> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    Ok(info.path)
}

/// Finds a worktree by name, matching branch name first, then directory name.
pub fn find(ctx: &Ctx, name: &str) -> Result<Option<WorktreeInfo>> {
    let infos = list(ctx)?;
    Ok(infos
        .iter()
        .find(|i| i.branch.as_deref() == Some(name))
        .or_else(|| infos.iter().find(|i| i.name == name))
        .cloned())
}

fn not_found(ctx: &Ctx, name: &str) -> anyhow::Error {
    let known = list(ctx)
        .map(|infos| {
            infos
                .iter()
                .map(|i| i.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    anyhow!("no worktree named '{name}' (known: {known})")
}

/// Display/addressing name for a worktree: its branch, or directory name when
/// detached.
fn worktree_name(branch: &Option<String>, path: &Path) -> String {
    branch.clone().unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    })
}

/// Branch names may contain `/`; flatten them so each worktree is a single
/// directory under the base.
fn sanitize_dir_name(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Copies `file` from the main worktree into the new worktree, preserving its
/// relative path. Missing sources are recorded as skipped, not errors.
fn copy_step(repo_root: &Path, worktree: &Path, file: &Path) -> SetupStep {
    let step = format!("copy {}", file.display());
    let src = repo_root.join(file);
    if !src.exists() {
        return SetupStep {
            step,
            ok: true,
            detail: Some("skipped: not present in main worktree".to_string()),
        };
    }
    let dst = worktree.join(file);
    let result = dst
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| std::fs::copy(&src, &dst).map(|_| ()));
    match result {
        Ok(()) => SetupStep {
            step,
            ok: true,
            detail: None,
        },
        Err(e) => SetupStep {
            step,
            ok: false,
            detail: Some(e.to_string()),
        },
    }
}

/// Runs one setup shell command inside the new worktree.
fn run_step(worktree: &Path, cmd: &str) -> SetupStep {
    let step = format!("run {cmd}");
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .output()
    {
        Ok(out) if out.status.success() => SetupStep {
            step,
            ok: true,
            detail: None,
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = format!(
                "exit {}: {}",
                out.status.code().map_or("?".to_string(), |c| c.to_string()),
                stderr.trim().chars().take(500).collect::<String>()
            );
            SetupStep {
                step,
                ok: false,
                detail: Some(detail),
            }
        }
        Err(e) => SetupStep {
            step,
            ok: false,
            detail: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_branch_dir_names() {
        assert_eq!(sanitize_dir_name("feature/login"), "feature-login");
        assert_eq!(sanitize_dir_name("plain"), "plain");
    }

    #[test]
    fn worktree_name_falls_back_to_dir() {
        assert_eq!(worktree_name(&Some("b".into()), Path::new("/x/y")), "b");
        assert_eq!(worktree_name(&None, Path::new("/x/y")), "y");
    }
}
