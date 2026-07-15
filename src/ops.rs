//! Core worktree operations shared by the CLI, TUI, and MCP server.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::config::Config;
use crate::conflict;
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

    /// Errors unless the repo has its own `.wtm.toml`. A global config alone
    /// does not count; every repo must be set up explicitly.
    pub fn ensure_initialized(&self) -> Result<()> {
        if self.repo_root.join(crate::config::CONFIG_FILE).exists() {
            return Ok(());
        }
        bail!(
            "this repository is not initialized for wtm; run `wtm init` (or plain `wtm` for \
             the interactive setup) first"
        )
    }

    /// `discover` plus the init check, for commands that require a set-up repo.
    pub fn discover_initialized(cwd: &Path) -> Result<Ctx> {
        let ctx = Ctx::discover(cwd)?;
        ctx.ensure_initialized()?;
        Ok(ctx)
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
    /// Remote ref the new branch was based on (e.g. "origin/feature") when a
    /// matching remote branch was pulled down; `None` for a fresh local branch.
    pub tracked_remote: Option<String>,
    pub setup: Vec<SetupStep>,
    /// True when every setup step succeeded.
    pub setup_ok: bool,
}

/// How `create` runs the configured setup commands.
pub enum RunMode {
    /// Capture output silently; used by `--json` and MCP where nothing is
    /// interactive.
    Capture,
    /// The child inherits the terminal, so output streams live and the user
    /// can answer prompts directly; used by the plain CLI.
    Inherit,
    /// Output is piped line-by-line through the progress callback, and the
    /// control can feed the command input or kill it; used by the TUI.
    Controlled(SetupControl),
}

/// Shared handle to the setup command currently run by `create`, letting
/// another thread (the TUI) send it input or kill it.
#[derive(Clone, Default)]
pub struct SetupControl {
    inner: Arc<Mutex<ControlInner>>,
}

#[derive(Default)]
struct ControlInner {
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    killed: bool,
}

impl SetupControl {
    /// Sends one line of input to the running setup command's stdin. Returns
    /// false when no command is running or its stdin has closed.
    pub fn send_line(&self, text: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.stdin.as_mut() {
            Some(stdin) => writeln!(stdin, "{text}")
                .and_then(|_| stdin.flush())
                .is_ok(),
            None => false,
        }
    }

    /// Kills the running setup command (its whole process group) and marks
    /// the create as aborted so remaining commands are skipped.
    pub fn kill(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.killed = true;
        if let Some(pid) = inner.pid {
            kill_process_group(pid);
        }
    }

    pub fn was_killed(&self) -> bool {
        self.inner.lock().unwrap().killed
    }

    /// Registers a just-spawned command. Returns false when a kill arrived
    /// before the spawn, in which case the caller must not run the command.
    fn attach(&self, stdin: Option<ChildStdin>, pid: u32) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.killed {
            return false;
        }
        inner.stdin = stdin;
        inner.pid = Some(pid);
        true
    }

    fn detach(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stdin = None;
        inner.pid = None;
    }
}

/// SIGKILLs the process group led by `pid` so shell children die with the
/// shell. Requires the child to have been spawned as a group leader.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--", &format!("-{pid}")])
        .output();
    #[cfg(not(unix))]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
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
/// called with a human-readable line before each long-running step (and with
/// every output line in `RunMode::Controlled`).
pub fn create(
    ctx: &Ctx,
    branch: &str,
    from: Option<&str>,
    mode: RunMode,
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
    let base = ctx.config.worktree_base(&ctx.repo_root)?;
    // Worktrees placed inside the repo would show up as untracked files in
    // every status/diff; keep them out via .git/info/exclude.
    if let Ok(rel) = base.strip_prefix(&ctx.repo_root) {
        exclude_from_git_status(&ctx.repo_root, rel)?;
    }
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
    // For a new branch with no explicit base, prefer a matching remote branch
    // so `wtm create feature` pulls down and tracks origin/feature instead of
    // branching from HEAD. An owned base string keeps the remote ref alive.
    let mut tracked_remote = None;
    let base: Option<String> = if create_branch {
        match from {
            Some(f) => Some(f.to_string()),
            None => match resolve_remote_branch(&ctx.repo_root, branch, &mut progress)? {
                Some(remote_ref) => {
                    tracked_remote = Some(remote_ref.clone());
                    Some(remote_ref)
                }
                None => Some("HEAD".to_string()),
            },
        }
    } else {
        None
    };
    git::worktree_add(&ctx.repo_root, &path, branch, base.as_deref())?;

    let mut setup = Vec::new();
    for file in &ctx.config.setup.copy {
        setup.push(copy_step(&ctx.repo_root, &path, file));
    }
    for cmd in &ctx.config.setup.run {
        progress(&format!("running: {cmd}"));
        let step = run_step(&path, cmd, &mode, &mut progress);
        let failed = !step.ok;
        setup.push(step);
        if failed {
            // Later commands often depend on earlier ones (e.g. npm install),
            // so stop rather than cascade failures.
            let aborted = matches!(&mode, RunMode::Controlled(c) if c.was_killed());
            let reason = if aborted {
                "skipped: setup aborted"
            } else {
                "skipped: earlier setup command failed"
            };
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
                    detail: Some(reason.to_string()),
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
        tracked_remote,
        setup,
        setup_ok,
    })
}

/// Finds a remote branch to base a new local tracking branch on. Already
/// fetched remote refs are checked first; only when none match and the repo
/// has remotes does it fetch and look again. Fetch failures (offline, auth)
/// are non-fatal so creating a fresh local branch still works.
fn resolve_remote_branch(
    repo_root: &Path,
    branch: &str,
    progress: &mut impl FnMut(&str),
) -> Result<Option<String>> {
    if let Some(remote_ref) = git::find_remote_ref(repo_root, branch)? {
        return Ok(Some(remote_ref));
    }
    if git::remotes(repo_root)?.is_empty() {
        return Ok(None);
    }
    progress(&format!(
        "fetching to look for a remote branch named '{branch}'"
    ));
    if git::fetch_all_prune(repo_root).is_ok() {
        return Ok(git::find_remote_ref(repo_root, branch)?);
    }
    Ok(None)
}

/// A directory already sitting where a new worktree for `branch` would go.
pub struct ExistingTarget {
    /// Absolute path of the conflicting directory.
    pub path: PathBuf,
    /// The name it is addressed by when it is already a registered worktree,
    /// so the caller can offer to open it instead of replacing it.
    pub worktree_name: Option<String>,
}

/// Absolute target path a worktree for `branch` would be created at (base dir
/// plus the sanitized branch name). Mirrors the path logic in `create`.
pub fn target_path(ctx: &Ctx, branch: &str) -> Result<PathBuf> {
    let base = ctx.config.worktree_base(&ctx.repo_root)?;
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    Ok(base.join(sanitize_dir_name(branch)))
}

/// Checks whether creating a worktree for `branch` would collide with an
/// existing directory, and whether that directory is already a worktree.
pub fn existing_target(ctx: &Ctx, branch: &str) -> Result<Option<ExistingTarget>> {
    let path = target_path(ctx, branch)?;
    if !path.exists() {
        return Ok(None);
    }
    let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let worktree_name = list(ctx)?.into_iter().find_map(|w| {
        let same = std::fs::canonicalize(&w.path)
            .map(|p| p == canon)
            .unwrap_or(false);
        same.then_some(w.name)
    });
    Ok(Some(ExistingTarget {
        path,
        worktree_name,
    }))
}

/// Removes whatever occupies `path` so a fresh worktree can take its place,
/// even when the directory is non-empty. Unregisters it from git when it is a
/// registered worktree (unlocking first, since a locked worktree is refused by
/// both `worktree remove` and `prune`), deletes the directory, then prunes
/// stale admin entries so a follow-up `worktree add` at this path succeeds.
///
/// A non-empty directory is never a reason to fail: `remove_dir_all` clears it,
/// and the git steps are best-effort with `prune` as the backstop.
pub fn remove_target(ctx: &Ctx, path: &Path) -> Result<()> {
    let canon = std::fs::canonicalize(path).ok();
    // Match against the path git actually recorded so removal/unlock target the
    // registration even when `path` is spelled differently (symlinks, etc.).
    let registered_path = git::list_worktrees(&ctx.repo_root)?
        .into_iter()
        .find(|w| std::fs::canonicalize(&w.path).ok() == canon && canon.is_some())
        .map(|w| w.path);
    if let Some(reg) = &registered_path {
        // Unlock first so the subsequent remove/prune can reclaim a locked
        // worktree; harmless (and ignored) when it was not locked.
        let _ = git::worktree_unlock(&ctx.repo_root, reg);
        // Best effort: if git still refuses we fall back to deleting the
        // directory and pruning below.
        let _ = git::worktree_remove(&ctx.repo_root, reg, true);
    }
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    git::worktree_prune(&ctx.repo_root)?;
    Ok(())
}

/// Whether the worktree occupying `path` holds work that replacing it would
/// lose: uncommitted changes, or commits on its branch that are not yet in the
/// repo's default branch. A plain directory that is not a registered worktree
/// (or a detached, clean one) is treated as having nothing to lose.
pub fn target_has_changes(ctx: &Ctx, path: &Path) -> Result<bool> {
    let canon = std::fs::canonicalize(path).ok();
    let Some(info) = list(ctx)?
        .into_iter()
        .find(|w| std::fs::canonicalize(&w.path).ok() == canon && canon.is_some())
    else {
        // Not a worktree, just a leftover directory: nothing to preserve.
        return Ok(false);
    };
    if info.dirty > 0 {
        return Ok(true);
    }
    // Only a branch can carry commits we can compare; a detached, clean
    // worktree has no branch tip to check against the default branch.
    let Some(branch) = info.branch.as_deref() else {
        return Ok(false);
    };
    let default = git::default_branch(&ctx.repo_root)?;
    if default == branch {
        return Ok(false);
    }
    Ok(git::commits_ahead_of(&ctx.repo_root, &default, branch)? > 0)
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
    if delete_branch && let Some(branch) = &info.branch {
        git::branch_delete(&ctx.repo_root, branch)?;
    }
    Ok(info)
}

/// True when the worktree named `name` has uncommitted changes.
pub fn worktree_is_dirty(ctx: &Ctx, name: &str) -> Result<bool> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    Ok(info.dirty > 0)
}

/// Stashes all changes (including untracked files) in the worktree named
/// `name`, so a subsequent removal can proceed without discarding the work.
pub fn stash_worktree(ctx: &Ctx, name: &str) -> Result<()> {
    stash_push(ctx, name, None).map(|_| ())
}

/// Removes just the worktree folder for `name`, never touching its branch.
/// Refuses on a dirty tree unless `force` (mirroring the guard in `remove`).
/// Returns the worktree info (including its branch name) so the caller can act
/// on the branch afterwards.
pub fn remove_worktree_only(ctx: &Ctx, name: &str, force: bool) -> Result<WorktreeInfo> {
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
    Ok(info)
}

/// Why a safe (`-d`) branch delete was refused, so a caller can offer the
/// matching recovery. `Deleted` means it actually succeeded.
pub enum DeleteBranchOutcome {
    /// The branch was deleted.
    Deleted,
    /// Refused: the branch is still checked out in another worktree (its name).
    CheckedOutElsewhere(String),
    /// Refused: the branch has commits not merged anywhere; `-D` would force it.
    NotMerged,
}

/// Attempts a safe (`-d`) delete of `branch`, reporting why git refused rather
/// than failing outright, so the interactive flow can offer a force retry.
/// Assumes the branch's own worktree has already been removed, so a checkout
/// means a genuinely different worktree, not the one being deleted.
pub fn try_delete_branch(ctx: &Ctx, branch: &str) -> Result<DeleteBranchOutcome> {
    if let Some(wt) = git::list_worktrees(&ctx.repo_root)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        return Ok(DeleteBranchOutcome::CheckedOutElsewhere(worktree_name(
            &wt.branch, &wt.path,
        )));
    }
    match git::branch_delete_flag(&ctx.repo_root, branch, false) {
        Ok(()) => Ok(DeleteBranchOutcome::Deleted),
        Err(e) if git::is_not_merged_error(&e) => Ok(DeleteBranchOutcome::NotMerged),
        Err(e) => Err(e.into()),
    }
}

/// Deletes the local branch `branch`, handling the two obstacles left once its
/// own worktree is gone:
///  - checked out in ANOTHER worktree: errors (non-force), or when `force`
///    switches that worktree to the repo's default branch first, then deletes.
///  - not fully merged: a non-force delete returns a clear "not fully merged"
///    error so the caller can offer to force; `force` uses `-D`.
pub fn delete_branch_maybe_force(ctx: &Ctx, branch: &str, force: bool) -> Result<()> {
    if let Some(wt) = git::list_worktrees(&ctx.repo_root)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        if !force {
            bail!(
                "branch '{branch}' is checked out at {}; remove that worktree first \
                 or force to move it to the default branch",
                wt.path.display()
            );
        }
        let default = git::default_branch(&ctx.repo_root)?;
        if default == branch {
            bail!(
                "branch '{branch}' is the repository's default branch and cannot be \
                 moved off its own worktree"
            );
        }
        // Move the other worktree onto the default branch so the branch is no
        // longer checked out anywhere and can be deleted.
        git::switch(&wt.path, &default)?;
    }
    match git::branch_delete_flag(&ctx.repo_root, branch, force) {
        Ok(()) => Ok(()),
        // Turn git's raw refusal into a message the interactive flow can act on.
        Err(e) if git::is_not_merged_error(&e) => Err(anyhow!(
            "branch '{branch}' is not fully merged; force to delete it anyway"
        )),
        Err(e) => Err(e.into()),
    }
}

/// Force-deletes `branch` (`-D`), first moving any other worktree that still
/// has it checked out onto the repository's default branch. Used by the TUI's
/// "Force" delete choice.
pub fn force_delete_branch(ctx: &Ctx, branch: &str) -> Result<()> {
    delete_branch_maybe_force(ctx, branch, true)
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

/// Unified diff of a single `path` within the worktree named `name`.
/// `untracked` should be true for files git doesn't track yet.
pub fn file_diff(ctx: &Ctx, name: &str, path: &str, untracked: bool) -> Result<String> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::diff_file(Path::new(&info.path), path, untracked).map_err(Into::into)
}

/// Discards uncommitted changes to `path` in the worktree named `name`,
/// restoring it to HEAD (or removing it if it was untracked).
pub fn revert_file(ctx: &Ctx, name: &str, path: &str, untracked: bool) -> Result<()> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::revert_file(Path::new(&info.path), path, untracked).map_err(Into::into)
}

/// Derives a `.gitignore` glob from a file path: `*.ext` when the file has an
/// extension, otherwise the bare file name (which git ignores at any depth).
pub fn ignore_pattern(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("*.{ext}"),
        _ => name.to_string(),
    }
}

/// Appends `pattern` on its own line to the `.gitignore` at the root of the
/// worktree named `name`, creating the file if it does not exist. Returns
/// `false` without writing when the exact pattern is already present.
pub fn add_to_gitignore(ctx: &Ctx, name: &str, pattern: &str) -> Result<bool> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let path = Path::new(&info.path).join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(false);
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(pattern);
    content.push('\n');
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Absolute path of the worktree named `name`.
pub fn path(ctx: &Ctx, name: &str) -> Result<String> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    Ok(info.path)
}

/// Result of `commit`.
#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    pub name: String,
    /// Abbreviated hash of the new commit.
    pub hash: String,
    /// Subject line of the new commit.
    pub summary: String,
    pub files_changed: usize,
}

/// Result of a stash push/pop/apply/drop action.
#[derive(Debug, Clone, Serialize)]
pub struct StashResult {
    pub name: String,
    /// The verb performed: "push", "pop", "apply", or "drop".
    pub action: String,
    /// Raw git output for the action.
    pub output: String,
}

/// Result of `stash list`.
#[derive(Debug, Clone, Serialize)]
pub struct StashListResult {
    pub name: String,
    pub entries: Vec<git::StashEntry>,
}

/// Result of `pull`.
#[derive(Debug, Clone, Serialize)]
pub struct PullResult {
    pub name: String,
    pub already_up_to_date: bool,
    /// Ahead/behind upstream after the pull.
    pub ahead_behind: Option<AheadBehind>,
}

/// Result of `push`.
#[derive(Debug, Clone, Serialize)]
pub struct PushResult {
    pub name: String,
    pub branch: String,
    /// True when the branch had no upstream and was published with `-u`.
    pub set_upstream: bool,
    /// Remote the branch was published to when `set_upstream` is true.
    pub remote: Option<String>,
}

/// Result of `fetch`.
#[derive(Debug, Clone, Serialize)]
pub struct FetchResult {
    /// Remotes that were fetched.
    pub remotes: Vec<String>,
}

/// One branch in `branch list`, enriched with worktree checkout info.
#[derive(Debug, Clone, Serialize)]
pub struct BranchListItem {
    pub name: String,
    /// Path of the worktree that has this branch checked out, if any.
    pub checked_out_path: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub subject: String,
    pub date: String,
}

/// Result of `branch list`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchListResult {
    pub branches: Vec<BranchListItem>,
}

/// Result of `branch create`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchCreateResult {
    pub name: String,
    /// Ref the branch was created from.
    pub from: String,
}

/// Result of `branch delete`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchDeleteResult {
    pub name: String,
    /// True when `-D` (force) was used instead of `-d`.
    pub forced: bool,
}

/// Result of `branch rename`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchRenameResult {
    pub old: String,
    pub new: String,
}

/// Result of `log`.
#[derive(Debug, Clone, Serialize)]
pub struct LogResult {
    pub name: String,
    pub entries: Vec<git::LogEntry>,
}

/// Outcome of `cherry_pick`, serialized with a `status` tag for `--json` output
/// and MCP results, mirroring [`MergeOutcome`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CherryPickOutcome {
    /// Every commit applied.
    Applied {
        /// Worktree the commits were applied into.
        target: String,
        /// How many commits were cherry-picked.
        count: usize,
        /// True when the commits were committed; false when loaded into the
        /// working tree only (`no_commit`).
        committed: bool,
    },
    /// A commit conflicted; the target worktree is left mid-cherry-pick so the
    /// listed files can be resolved there, then continued.
    Conflicted {
        /// Worktree left mid-cherry-pick.
        target: String,
        /// Paths of the conflicted files.
        files: Vec<String>,
    },
}

/// Outcome of `stash_pop`, serialized with a `status` tag. A clean pop drops the
/// stash; a conflicting pop keeps it and leaves files to resolve.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StashPopOutcome {
    /// The stash applied cleanly and was dropped.
    Applied {
        /// Worktree the stash was popped in.
        name: String,
        /// Raw git output for the pop.
        output: String,
    },
    /// Applying the stash produced conflicts; the stash was NOT dropped. Resolve
    /// the listed files, then drop the stash to finish.
    Conflicted {
        /// Worktree left with conflicts.
        name: String,
        /// Stash entry that stayed in place (the one that was popped).
        index: Option<u32>,
        /// Paths of the conflicted files.
        files: Vec<String>,
    },
}

/// Outcome of `merge`/`update`, serialized with a `status` tag for `--json`
/// output and MCP results.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MergeOutcome {
    /// The target branch already contained the source; nothing changed.
    UpToDate,
    /// The merge completed; `commit` is the short hash of the target's new HEAD.
    Clean { commit: String },
    /// The merge stopped on conflicts; the target worktree is left mid-merge
    /// so the listed files can be resolved there.
    Conflicted { files: Vec<String> },
}

/// Which in-progress operation a set of conflicts belongs to, so the resolver's
/// "complete"/"abort" can dispatch correctly. Merge and cherry-pick leave a
/// marker in the repo (MERGE_HEAD / CHERRY_PICK_HEAD) and finish by continuing
/// that sequence; a stash pop leaves no marker, so finishing means dropping the
/// applied stash entry (no new commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolveKind {
    Merge,
    CherryPick,
    StashPop {
        /// Stash entry to drop on completion (the one that was popped).
        index: Option<u32>,
    },
}

/// Result of `switch`.
#[derive(Debug, Clone, Serialize)]
pub struct SwitchResult {
    /// The worktree that switched (addressed by its new branch name).
    pub name: String,
    /// The branch now checked out.
    pub branch: String,
    /// Absolute path of the worktree.
    pub path: String,
}

/// Stages and commits changes in the worktree named `name`. Stages everything
/// by default, or only `paths` when given. Refuses when nothing is staged.
pub fn commit(
    ctx: &Ctx,
    name: &str,
    message: &str,
    paths: Option<&[String]>,
) -> Result<CommitResult> {
    if message.trim().is_empty() {
        bail!("commit message must not be empty");
    }
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let dir = Path::new(&info.path);
    match paths {
        Some(paths) if !paths.is_empty() => git::stage_paths(dir, paths)?,
        _ => git::stage_all(dir)?,
    }
    if !git::has_staged_changes(dir)? {
        bail!("nothing to commit in worktree '{}'", info.name);
    }
    git::commit(dir, message)?;
    Ok(CommitResult {
        name: info.name,
        hash: git::short_hash(dir)?,
        summary: git::head_subject(dir)?,
        files_changed: git::head_files_changed(dir)?,
    })
}

/// Stashes changes (including untracked files) in the worktree named `name`.
pub fn stash_push(ctx: &Ctx, name: &str, message: Option<&str>) -> Result<StashResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let output = git::stash_push(Path::new(&info.path), message)?;
    Ok(StashResult {
        name: info.name,
        action: "push".to_string(),
        output,
    })
}

/// Stashes only `paths` in the worktree named `name`, leaving the rest of the
/// working tree in place.
pub fn stash_push_paths(
    ctx: &Ctx,
    name: &str,
    paths: &[String],
    message: Option<&str>,
) -> Result<StashResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let output = git::stash_push_paths(Path::new(&info.path), paths, message)?;
    Ok(StashResult {
        name: info.name,
        action: "push".to_string(),
        output,
    })
}

/// Lists stash entries for the worktree named `name`.
pub fn stash_list(ctx: &Ctx, name: &str) -> Result<StashListResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let entries = git::stash_list(Path::new(&info.path))?;
    Ok(StashListResult {
        name: info.name,
        entries,
    })
}

/// Pops a stash entry (default most recent) in the worktree named `name`. A
/// conflicting pop keeps the stash and returns [`StashPopOutcome::Conflicted`]
/// so the caller can route the conflicts into the resolver; finishing means
/// resolving each file then dropping the stash.
pub fn stash_pop(ctx: &Ctx, name: &str, index: Option<u32>) -> Result<StashPopOutcome> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    match git::stash_pop(Path::new(&info.path), index)? {
        git::StashPopStatus::Applied(output) => Ok(StashPopOutcome::Applied {
            name: info.name,
            output,
        }),
        git::StashPopStatus::Conflicted(files) => Ok(StashPopOutcome::Conflicted {
            name: info.name,
            index,
            files,
        }),
    }
}

/// Applies a stash entry (default most recent) in the worktree named `name`.
pub fn stash_apply(ctx: &Ctx, name: &str, index: Option<u32>) -> Result<StashResult> {
    stash_action(ctx, name, "apply", index, git::stash_apply)
}

/// Drops a stash entry (default most recent) in the worktree named `name`.
pub fn stash_drop(ctx: &Ctx, name: &str, index: Option<u32>) -> Result<StashResult> {
    stash_action(ctx, name, "drop", index, git::stash_drop)
}

/// Shared body for stash pop/apply/drop: resolves the worktree then runs the
/// given git operation on an optional entry index.
fn stash_action(
    ctx: &Ctx,
    name: &str,
    action: &str,
    index: Option<u32>,
    op: fn(&Path, Option<u32>) -> git::Result<String>,
) -> Result<StashResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let output = op(Path::new(&info.path), index)?;
    Ok(StashResult {
        name: info.name,
        action: action.to_string(),
        output,
    })
}

/// Pulls the worktree named `name`. Fast-forward only unless `rebase`. Errors
/// clearly when the branch has no upstream configured.
pub fn pull(ctx: &Ctx, name: &str, rebase: bool) -> Result<PullResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let dir = Path::new(&info.path);
    if !git::has_upstream(dir) {
        bail!(
            "worktree '{}' has no upstream configured; push it first or set one \
             with `git branch --set-upstream-to`",
            info.name
        );
    }
    let output = git::pull(dir, rebase)?;
    let already_up_to_date = output.contains("Already up to date");
    let ahead_behind = git::ahead_behind(dir)?;
    Ok(PullResult {
        name: info.name,
        already_up_to_date,
        ahead_behind,
    })
}

/// Pushes the worktree named `name`. When the branch has no upstream it is
/// published to origin with `-u` automatically.
pub fn push(ctx: &Ctx, name: &str, force_with_lease: bool) -> Result<PushResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let dir = Path::new(&info.path);
    let branch = info
        .branch
        .clone()
        .ok_or_else(|| anyhow!("worktree '{}' is detached; cannot push", info.name))?;
    if git::has_upstream(dir) {
        git::push(dir, force_with_lease)?;
        Ok(PushResult {
            name: info.name,
            branch,
            set_upstream: false,
            remote: None,
        })
    } else {
        let remote = "origin";
        git::push_set_upstream(dir, remote, &branch, force_with_lease)?;
        Ok(PushResult {
            name: info.name,
            branch,
            set_upstream: true,
            remote: Some(remote.to_string()),
        })
    }
}

/// Fetches every remote for the repo and prunes deleted remote branches.
pub fn fetch(ctx: &Ctx) -> Result<FetchResult> {
    git::fetch_all_prune(&ctx.repo_root)?;
    Ok(FetchResult {
        remotes: git::remotes(&ctx.repo_root)?,
    })
}

/// Lists local branches with tracking info and, for each, the worktree it is
/// checked out in (if any).
pub fn branch_list(ctx: &Ctx) -> Result<BranchListResult> {
    let details = git::branch_details(&ctx.repo_root)?;
    let worktrees = git::list_worktrees(&ctx.repo_root)?;
    let branches = details
        .into_iter()
        .map(|d| {
            let checked_out_path = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(&d.name))
                .map(|w| w.path.to_string_lossy().to_string());
            BranchListItem {
                name: d.name,
                checked_out_path,
                upstream: d.upstream,
                ahead: d.ahead,
                behind: d.behind,
                subject: d.subject,
                date: d.date,
            }
        })
        .collect();
    Ok(BranchListResult { branches })
}

/// Creates a branch (without a worktree), optionally from `from`.
pub fn branch_create(ctx: &Ctx, name: &str, from: Option<&str>) -> Result<BranchCreateResult> {
    if name.trim().is_empty() {
        bail!("branch name must not be empty");
    }
    if git::branch_exists(&ctx.repo_root, name) {
        bail!("branch '{name}' already exists");
    }
    git::branch_create(&ctx.repo_root, name, from)?;
    Ok(BranchCreateResult {
        name: name.to_string(),
        from: from.unwrap_or("HEAD").to_string(),
    })
}

/// Deletes a branch. Refuses when the branch is checked out in any worktree;
/// `force` uses `-D` to delete even unmerged branches.
pub fn branch_delete(ctx: &Ctx, name: &str, force: bool) -> Result<BranchDeleteResult> {
    let worktrees = git::list_worktrees(&ctx.repo_root)?;
    if let Some(wt) = worktrees.iter().find(|w| w.branch.as_deref() == Some(name)) {
        bail!(
            "branch '{name}' is checked out at {}; remove that worktree first",
            wt.path.display()
        );
    }
    git::branch_delete_flag(&ctx.repo_root, name, force)?;
    Ok(BranchDeleteResult {
        name: name.to_string(),
        forced: force,
    })
}

/// Renames branch `old` to `new`.
pub fn branch_rename(ctx: &Ctx, old: &str, new: &str) -> Result<BranchRenameResult> {
    git::branch_rename(&ctx.repo_root, old, new)?;
    Ok(BranchRenameResult {
        old: old.to_string(),
        new: new.to_string(),
    })
}

/// Recent commits for the worktree named `name` (newest first).
pub fn log(ctx: &Ctx, name: &str, count: u32) -> Result<LogResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let entries = git::log(Path::new(&info.path), count)?;
    Ok(LogResult {
        name: info.name,
        entries,
    })
}

/// Recent commits reachable from a local branch (newest first), without
/// checking it out. Used by the Branches tab to show a branch's history for
/// cherry-picking. Commit hashes are full so they can be passed to
/// [`cherry_pick`].
pub fn branch_log(ctx: &Ctx, branch: &str, count: u32) -> Result<LogResult> {
    if !git::branch_exists(&ctx.repo_root, branch) {
        bail!("no local branch named '{branch}'");
    }
    let entries = git::log_ref(&ctx.repo_root, branch, count)?;
    Ok(LogResult {
        name: branch.to_string(),
        entries,
    })
}

/// The same history as [`log`], drawn as a commit graph. Used by the TUI's tree
/// view; see [`git::log_graph`].
pub fn log_graph(ctx: &Ctx, name: &str, count: u32) -> Result<Vec<git::GraphLine>> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    Ok(git::log_graph(Path::new(&info.path), None, count, false)?)
}

/// The same history as [`branch_log`], drawn as a commit graph. Hashes are full
/// so the graph's commits can be passed to [`cherry_pick`] like `branch_log`'s.
pub fn branch_log_graph(ctx: &Ctx, branch: &str, count: u32) -> Result<Vec<git::GraphLine>> {
    if !git::branch_exists(&ctx.repo_root, branch) {
        bail!("no local branch named '{branch}'");
    }
    Ok(git::log_graph(&ctx.repo_root, Some(branch), count, true)?)
}

/// Result of `branch_pull`.
#[derive(Debug, Clone, Serialize)]
pub struct BranchPullResult {
    pub branch: String,
    /// True when the branch was already at its upstream.
    pub already_up_to_date: bool,
    /// Worktree the branch was pulled in, when it is checked out somewhere.
    pub worktree: Option<String>,
}

/// Fast-forwards a local branch to its upstream. When the branch is checked out
/// in a worktree this is an ordinary `git pull --ff-only` there (so the working
/// tree moves with the branch); otherwise the ref is fast-forwarded in place
/// without a checkout. Either way a diverged branch fails rather than merging.
pub fn branch_pull(ctx: &Ctx, branch: &str) -> Result<BranchPullResult> {
    if !git::branch_exists(&ctx.repo_root, branch) {
        bail!("no local branch named '{branch}'");
    }
    let Some((remote, remote_branch)) = git::branch_upstream(&ctx.repo_root, branch)? else {
        bail!(
            "branch '{branch}' has no upstream configured; push it first or set one \
             with `git branch --set-upstream-to`"
        );
    };
    let worktrees = git::list_worktrees(&ctx.repo_root)?;
    if let Some(wt) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        let output = git::pull(&wt.path, false)?;
        return Ok(BranchPullResult {
            branch: branch.to_string(),
            already_up_to_date: output.contains("Already up to date"),
            worktree: Some(worktree_name(&wt.branch, &wt.path)),
        });
    }
    let before = git::run(&ctx.repo_root, &["rev-parse", branch])?;
    git::fetch_into_branch(&ctx.repo_root, &remote, &remote_branch, branch)?;
    let after = git::run(&ctx.repo_root, &["rev-parse", branch])?;
    Ok(BranchPullResult {
        branch: branch.to_string(),
        already_up_to_date: before == after,
        worktree: None,
    })
}

/// Cherry-picks `commits` into the worktree named `target`. `commits` are taken
/// oldest-first (the order git applies them). With `no_commit` the changes are
/// staged in the target worktree without a commit so they can be reviewed or
/// edited; otherwise each commit is recorded with its original message. A
/// conflict leaves the target worktree mid-cherry-pick (see
/// [`CherryPickOutcome::Conflicted`]) so the conflicts can be resolved in place
/// and the sequence continued.
pub fn cherry_pick(
    ctx: &Ctx,
    target: &str,
    commits: &[String],
    no_commit: bool,
) -> Result<CherryPickOutcome> {
    if commits.is_empty() {
        bail!("no commits to cherry-pick");
    }
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    match git::cherry_pick(Path::new(&info.path), commits, no_commit)? {
        git::CherryPickStatus::Applied => Ok(CherryPickOutcome::Applied {
            target: info.name,
            count: commits.len(),
            committed: !no_commit,
        }),
        git::CherryPickStatus::Conflicted(files) => Ok(CherryPickOutcome::Conflicted {
            target: info.name,
            files,
        }),
    }
}

/// Merges local branch `source_branch` into the branch checked out in the
/// worktree named `target`, running the merge inside that worktree. `no_ff`
/// forces a merge commit even when a fast-forward would do. On a conflict the
/// worktree is left mid-merge (see [`MergeOutcome::Conflicted`]) so the
/// conflicts can be resolved in place; `git::merge_abort` and
/// `git::merge_continue` finish it either way.
pub fn merge(ctx: &Ctx, target: &str, source_branch: &str, no_ff: bool) -> Result<MergeOutcome> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    if !git::branch_exists(&ctx.repo_root, source_branch) {
        bail!("no local branch named '{source_branch}'");
    }
    if info.branch.as_deref() == Some(source_branch) {
        bail!(
            "worktree '{}' already has '{source_branch}' checked out; nothing to merge",
            info.name
        );
    }
    let dir = Path::new(&info.path);
    match git::merge(dir, source_branch, no_ff)? {
        git::MergeStatus::AlreadyUpToDate => Ok(MergeOutcome::UpToDate),
        git::MergeStatus::Merged => Ok(MergeOutcome::Clean {
            commit: git::short_hash(dir)?,
        }),
        git::MergeStatus::Conflicted(files) => Ok(MergeOutcome::Conflicted { files }),
    }
}

/// Merges the repository's default branch into the worktree named `target`,
/// bringing its branch up to date with the mainline. Errors when the target
/// already has the default branch checked out.
pub fn update(ctx: &Ctx, target: &str) -> Result<MergeOutcome> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let default = git::default_branch(&ctx.repo_root)?;
    if info.branch.as_deref() == Some(default.as_str()) {
        bail!(
            "worktree '{}' has the default branch '{default}' checked out; \
             there is nothing to update it from",
            info.name
        );
    }
    merge(ctx, target, &default, false)
}

/// A conflicted file's contents, parsed into segments, ready for a resolver
/// to inspect or act on.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictFile {
    /// Path relative to the worktree root.
    pub path: String,
    pub segments: Vec<conflict::ConflictSegment>,
    /// Label for "our" side, e.g. the branch checked out in the target worktree.
    pub ours_label: String,
    /// Label for "their" side, e.g. the branch being merged in.
    pub theirs_label: String,
}

/// Result of `complete_resolution`.
#[derive(Debug, Clone, Serialize)]
pub struct CompleteResolutionResult {
    pub target: String,
    /// Short hash of the new commit for a merge/cherry-pick; `None` for a stash
    /// pop, which finishes by dropping the stash without committing.
    pub commit: Option<String>,
}

/// Conflicted (unmerged) files in the worktree named `target`.
pub fn list_conflicts(ctx: &Ctx, target: &str) -> Result<Vec<String>> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    git::conflicted_files(Path::new(&info.path)).map_err(Into::into)
}

/// Reads and parses the conflicted file at `path` (relative to the worktree
/// root) in the worktree named `target`. `ours_label`/`theirs_label` are taken
/// from the file's own conflict markers when git wrote them there, falling
/// back to the worktree's checked-out branch and the short hash of
/// `MERGE_HEAD` respectively.
pub fn read_conflict(ctx: &Ctx, target: &str, path: &str) -> Result<ConflictFile> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    let full = dir.join(path);
    let text =
        std::fs::read_to_string(&full).with_context(|| format!("reading {}", full.display()))?;
    let (marker_ours, marker_theirs) = conflict::marker_labels(&text);
    let ours_label =
        marker_ours.unwrap_or_else(|| info.branch.clone().unwrap_or_else(|| "HEAD".to_string()));
    let theirs_label = marker_theirs
        .or_else(|| git::run(dir, &["rev-parse", "--short", "MERGE_HEAD"]).ok())
        .unwrap_or_else(|| "MERGE_HEAD".to_string());
    Ok(ConflictFile {
        path: path.to_string(),
        segments: conflict::parse(&text),
        ours_label,
        theirs_label,
    })
}

/// Writes `resolved_text` to `path` in the worktree named `target` and stages
/// it, marking that file's conflict resolved.
pub fn write_resolution(ctx: &Ctx, target: &str, path: &str, resolved_text: &str) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    let full = dir.join(path);
    std::fs::write(&full, resolved_text).with_context(|| format!("writing {}", full.display()))?;
    git::stage_paths(dir, &[path.to_string()])?;
    Ok(())
}

/// Resolves the conflict at `path` in the worktree named `target` by taking
/// "our" side whole, then stages it.
pub fn checkout_ours(ctx: &Ctx, target: &str, path: &str) -> Result<()> {
    checkout_conflict_side(ctx, target, path, true)
}

/// Resolves the conflict at `path` in the worktree named `target` by taking
/// "their" side whole, then stages it.
pub fn checkout_theirs(ctx: &Ctx, target: &str, path: &str) -> Result<()> {
    checkout_conflict_side(ctx, target, path, false)
}

/// Shared body for `checkout_ours`/`checkout_theirs`.
fn checkout_conflict_side(ctx: &Ctx, target: &str, path: &str, ours: bool) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    git::checkout_conflict_side(dir, path, ours)?;
    git::stage_paths(dir, &[path.to_string()])?;
    Ok(())
}

/// Detects which in-progress operation left the worktree named `target` with
/// conflicts, by inspecting the repo's merge/cherry-pick markers. Returns `None`
/// when neither is present (a stash pop leaves no marker, so callers that know a
/// stash pop is being resolved supply that kind themselves).
pub fn detect_resolve_kind(ctx: &Ctx, target: &str) -> Result<Option<ResolveKind>> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    if git::is_merging(dir) {
        Ok(Some(ResolveKind::Merge))
    } else if git::is_cherry_picking(dir) {
        Ok(Some(ResolveKind::CherryPick))
    } else {
        Ok(None)
    }
}

/// Finishes an in-progress conflict resolution in the worktree named `target`
/// once every conflict has been staged, dispatching on `kind`: a merge commits
/// (using `message` when given, otherwise git's prepared message); a cherry-pick
/// continues its sequence (recording the original message; `message` is ignored,
/// as `--continue` reuses the picked commit's message); a stash pop drops the
/// applied stash entry without committing. Refuses when conflicts remain, or
/// when the expected merge/cherry-pick is not actually in progress.
pub fn complete_resolution(
    ctx: &Ctx,
    target: &str,
    kind: ResolveKind,
    message: Option<&str>,
) -> Result<CompleteResolutionResult> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    let remaining = git::conflicted_files(dir)?;
    if !remaining.is_empty() {
        bail!(
            "worktree '{}' still has unresolved conflicts: {}",
            info.name,
            remaining.join(", ")
        );
    }
    let commit = match kind {
        ResolveKind::Merge => {
            if !git::is_merging(dir) {
                bail!("worktree '{}' has no merge in progress", info.name);
            }
            match message {
                Some(msg) => git::commit(dir, msg)?,
                None => git::merge_continue(dir)?,
            }
            Some(git::short_hash(dir)?)
        }
        ResolveKind::CherryPick => {
            if !git::is_cherry_picking(dir) {
                bail!("worktree '{}' has no cherry-pick in progress", info.name);
            }
            git::cherry_pick_continue(dir)?;
            Some(git::short_hash(dir)?)
        }
        ResolveKind::StashPop { index } => {
            // A stash pop applies to the working tree with no commit; finishing
            // is simply dropping the stash the conflicting pop left behind.
            git::stash_drop(dir, index)?;
            None
        }
    };
    Ok(CompleteResolutionResult {
        target: info.name,
        commit,
    })
}

/// Abandons an in-progress conflict resolution in the worktree named `target`,
/// dispatching on `kind`: merge and cherry-pick run their `--abort`; a stash pop
/// discards the conflicting application (reset to HEAD) while keeping the stash
/// entry, so the stashed work is not lost.
pub fn abort_resolution(ctx: &Ctx, target: &str, kind: ResolveKind) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    match kind {
        ResolveKind::Merge => git::merge_abort(dir)?,
        ResolveKind::CherryPick => git::cherry_pick_abort(dir)?,
        ResolveKind::StashPop { .. } => git::reset_hard(dir)?,
    }
    Ok(())
}

/// Switches the worktree named `name` to check out the existing local branch
/// `branch`. Refuses when the branch is already checked out in another worktree
/// (git forbids this) or is already the worktree's current branch.
pub fn switch_branch(ctx: &Ctx, name: &str, branch: &str) -> Result<SwitchResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    if info.branch.as_deref() == Some(branch) {
        bail!(
            "worktree '{}' already has '{branch}' checked out",
            info.name
        );
    }
    if !git::branch_exists(&ctx.repo_root, branch) {
        bail!("no local branch named '{branch}'");
    }
    if let Some(other) = list(ctx)?
        .into_iter()
        .find(|i| i.path != info.path && i.branch.as_deref() == Some(branch))
    {
        bail!(
            "branch '{branch}' is already checked out in worktree '{}'",
            other.name
        );
    }
    git::switch(Path::new(&info.path), branch)?;
    Ok(SwitchResult {
        name: branch.to_string(),
        branch: branch.to_string(),
        path: info.path,
    })
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

/// Adds `/<rel>/` to `.git/info/exclude` (creating it if needed) so a
/// worktree directory inside the repo stays out of `git status`. Skipped for
/// unusual layouts where `.git` isn't a directory.
fn exclude_from_git_status(repo_root: &Path, rel: &Path) -> Result<()> {
    if rel.as_os_str().is_empty() || !repo_root.join(".git").is_dir() {
        return Ok(());
    }
    let line = format!("/{}/", rel.display());
    let info = repo_root.join(".git").join("info");
    let exclude = info.join("exclude");
    let mut content = std::fs::read_to_string(&exclude).unwrap_or_default();
    if content.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    std::fs::create_dir_all(&info)
        .with_context(|| format!("failed to create {}", info.display()))?;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&exclude, content)
        .with_context(|| format!("failed to update {}", exclude.display()))?;
    Ok(())
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

/// Runs one setup shell command inside the new worktree, dispatching on how
/// the caller wants output and input handled.
fn run_step(
    worktree: &Path,
    cmd: &str,
    mode: &RunMode,
    progress: &mut impl FnMut(&str),
) -> SetupStep {
    match mode {
        RunMode::Capture => run_step_captured(worktree, cmd),
        RunMode::Inherit => run_step_inherited(worktree, cmd),
        RunMode::Controlled(control) => run_step_controlled(worktree, cmd, control, progress),
    }
}

fn step_ok(step: String) -> SetupStep {
    SetupStep {
        step,
        ok: true,
        detail: None,
    }
}

fn step_failed(step: String, detail: String) -> SetupStep {
    SetupStep {
        step,
        ok: false,
        detail: Some(detail),
    }
}

/// Runs a setup command with captured output (nothing shown, nothing asked).
fn run_step_captured(worktree: &Path, cmd: &str) -> SetupStep {
    let step = format!("run {cmd}");
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .output()
    {
        Ok(out) if out.status.success() => step_ok(step),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = format!(
                "exit {}: {}",
                out.status.code().map_or("?".to_string(), |c| c.to_string()),
                stderr.trim().chars().take(500).collect::<String>()
            );
            step_failed(step, detail)
        }
        Err(e) => step_failed(step, e.to_string()),
    }
}

/// Runs a setup command attached to the terminal: output streams live and
/// prompts read from the user's stdin.
fn run_step_inherited(worktree: &Path, cmd: &str) -> SetupStep {
    let step = format!("run {cmd}");
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .status()
    {
        Ok(status) if status.success() => step_ok(step),
        Ok(status) => step_failed(
            step,
            format!(
                "exit {}",
                status.code().map_or("?".to_string(), |c| c.to_string())
            ),
        ),
        Err(e) => step_failed(step, e.to_string()),
    }
}

/// Runs a setup command with piped stdio: every output line goes through
/// `progress`, input arrives via the control, and a kill via the control
/// takes down the whole process group.
fn run_step_controlled(
    worktree: &Path,
    cmd: &str,
    control: &SetupControl,
    progress: &mut impl FnMut(&str),
) -> SetupStep {
    let step = format!("run {cmd}");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Make the shell a process group leader so kill() reaches its children
    // (package managers spawn deep trees).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return step_failed(step, e.to_string()),
    };
    let stdin = child.stdin.take();
    if !control.attach(stdin, child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return step_failed(step, "aborted by user".to_string());
    }

    // One channel carries both streams so lines appear roughly in order.
    let (tx, rx) = channel::<(bool, String)>();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let _ = tx.send((false, line));
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                let _ = tx.send((true, line));
            }
        }));
    }
    // Drop the original sender so the drain loop ends when the readers do.
    drop(tx);
    let mut stderr_tail: Vec<String> = Vec::new();
    for (is_stderr, line) in rx {
        progress(&line);
        if is_stderr {
            if stderr_tail.len() >= 5 {
                stderr_tail.remove(0);
            }
            stderr_tail.push(line);
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait();
    control.detach();

    if control.was_killed() {
        return step_failed(step, "aborted by user".to_string());
    }
    match status {
        Ok(status) if status.success() => step_ok(step),
        Ok(status) => step_failed(
            step,
            format!(
                "exit {}: {}",
                status.code().map_or("?".to_string(), |c| c.to_string()),
                stderr_tail
                    .join(" | ")
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        ),
        Err(e) => step_failed(step, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway initialized repo with one commit on `main` and a
    /// hand-made Ctx (default config), so the developer's global config can't
    /// leak in. Returns the temp dir plus the Ctx.
    fn temp_ctx() -> (tempfile::TempDir, Ctx) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@e.st"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            git(&repo, &args);
        }
        std::fs::write(repo.join(".wtm.toml"), "").unwrap();
        let ctx = Ctx {
            repo_root: git::repo_root(&repo).unwrap(),
            config: Config::default(),
        };
        (tmp, ctx)
    }

    /// Runs a git command in `dir`, asserting it succeeds.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Creates a worktree for `branch` with no setup steps, returning its path.
    fn make_worktree(ctx: &Ctx, branch: &str) -> PathBuf {
        let r = create(ctx, branch, None, RunMode::Capture, |_| {}).unwrap();
        PathBuf::from(r.path)
    }

    #[test]
    fn removes_worktree_and_merged_branch() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feature");
        // Merged branch (no new commits): folder removal then a safe delete
        // should take out both.
        let info = remove_worktree_only(&ctx, "feature", false).unwrap();
        assert_eq!(info.branch.as_deref(), Some("feature"));
        assert!(matches!(
            try_delete_branch(&ctx, "feature").unwrap(),
            DeleteBranchOutcome::Deleted
        ));
        assert!(!git::branch_exists(&ctx.repo_root, "feature"));
    }

    #[test]
    fn unmerged_branch_is_refused_then_force_deleted() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "wip");
        // Add a commit that lives only on `wip`, so a safe delete is refused.
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "wip work"]);

        let _ = remove_worktree_only(&ctx, "wip", false).unwrap();
        assert!(matches!(
            try_delete_branch(&ctx, "wip").unwrap(),
            DeleteBranchOutcome::NotMerged
        ));
        assert!(git::branch_exists(&ctx.repo_root, "wip"));
        // Forcing (-D) takes it out.
        force_delete_branch(&ctx, "wip").unwrap();
        assert!(!git::branch_exists(&ctx.repo_root, "wip"));
    }

    #[test]
    fn force_delete_switches_worktree_checked_out_elsewhere() {
        let (_tmp, ctx) = temp_ctx();
        // Free up `main` so it can be switched onto: move the main worktree to a
        // separate `trunk` branch. `default_branch` still resolves to `main`.
        git(&ctx.repo_root, &["switch", "-c", "trunk"]);
        let path = make_worktree(&ctx, "feat");

        // `feat` is checked out in its worktree; a non-force delete is refused.
        assert!(matches!(
            try_delete_branch(&ctx, "feat").unwrap(),
            DeleteBranchOutcome::CheckedOutElsewhere(_)
        ));
        // Forcing moves that worktree to the default branch, then deletes.
        force_delete_branch(&ctx, "feat").unwrap();
        assert!(!git::branch_exists(&ctx.repo_root, "feat"));
        let wts = git::list_worktrees(&ctx.repo_root).unwrap();
        let moved = wts.iter().find(|w| w.path == path).unwrap();
        assert_eq!(moved.branch.as_deref(), Some("main"));
    }

    #[test]
    fn remove_target_clears_non_empty_worktree_and_reuses_path() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        // Populate the worktree with untracked files so the directory is not
        // empty; a naive rmdir would fail here.
        std::fs::write(path.join("a.txt"), "x\n").unwrap();
        std::fs::create_dir(path.join("sub")).unwrap();
        std::fs::write(path.join("sub/b.txt"), "y\n").unwrap();

        remove_target(&ctx, &path).unwrap();
        assert!(!path.exists(), "directory should be gone");
        // No worktree should remain registered at that path.
        let still_registered = git::list_worktrees(&ctx.repo_root)
            .unwrap()
            .iter()
            .any(|w| w.path == path);
        assert!(!still_registered, "path should be unregistered");
        // The path is reusable: a fresh worktree can be created there.
        let r = create(&ctx, "feature2", None, RunMode::Capture, |_| {}).unwrap();
        assert_eq!(PathBuf::from(&r.path).file_name().unwrap(), "feature2");
    }

    #[test]
    fn remove_target_reclaims_locked_worktree() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(path.join("dirty.txt"), "x\n").unwrap();
        // A locked worktree is refused by `worktree remove --force` (single
        // force) and skipped by `prune`; remove_target must still reclaim it.
        git(
            &ctx.repo_root,
            &["worktree", "lock", path.to_str().unwrap()],
        );

        remove_target(&ctx, &path).unwrap();
        assert!(!path.exists());
        // The path is reusable afterwards.
        let path2 = make_worktree(&ctx, "feature3");
        assert!(path2.exists());
    }

    #[test]
    fn target_has_changes_false_when_clean_and_merged() {
        let (_tmp, ctx) = temp_ctx();
        // A fresh worktree off HEAD: clean and fully merged into main.
        let path = make_worktree(&ctx, "feature");
        assert!(!target_has_changes(&ctx, &path).unwrap());
    }

    #[test]
    fn target_has_changes_true_when_dirty() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        assert!(target_has_changes(&ctx, &path).unwrap());
    }

    #[test]
    fn target_has_changes_true_with_unmerged_commit() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        // A commit only on `feature`, not yet in `main`: replacing loses it.
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "unique work"]);
        assert!(target_has_changes(&ctx, &path).unwrap());
    }

    #[test]
    fn target_has_changes_false_for_plain_directory() {
        let (_tmp, ctx) = temp_ctx();
        // A directory that is not a registered worktree: nothing to preserve.
        let dir = ctx.repo_root.join("..").join("just-a-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "x\n").unwrap();
        assert!(!target_has_changes(&ctx, &dir).unwrap());
    }

    #[test]
    fn merge_clean_then_up_to_date() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        // Non-overlapping changes: one file on main, another on feature.
        std::fs::write(ctx.repo_root.join("main.txt"), "m\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "main work"]);
        std::fs::write(path.join("feat.txt"), "f\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "feature work"]);

        let outcome = merge(&ctx, "feature", "main", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        assert!(path.join("main.txt").exists());
        assert!(!git::is_merging(&path));

        // A second merge has nothing new to bring in.
        let outcome = merge(&ctx, "feature", "main", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::UpToDate), "{outcome:?}");
    }

    #[test]
    fn merge_conflict_leaves_tree_mid_merge_and_abort_recovers() {
        let (_tmp, ctx) = temp_ctx();
        // Both branches edit the same line of the same file.
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(&ctx, "feature");
        std::fs::write(ctx.repo_root.join("shared.txt"), "main version\n").unwrap();
        git(&ctx.repo_root, &["commit", "-am", "main edit"]);
        std::fs::write(path.join("shared.txt"), "feature version\n").unwrap();
        git(&path, &["commit", "-am", "feature edit"]);

        let outcome = merge(&ctx, "feature", "main", false).unwrap();
        let MergeOutcome::Conflicted { files } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        assert_eq!(files, vec!["shared.txt".to_string()]);
        // The worktree must be left mid-merge so a resolver can take over.
        assert!(git::is_merging(&path));
        assert_eq!(git::conflicted_files(&path).unwrap(), files);

        git::merge_abort(&path).unwrap();
        assert!(!git::is_merging(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\n"
        );
    }

    #[test]
    fn merge_continue_commits_a_resolved_conflict() {
        let (_tmp, ctx) = temp_ctx();
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(&ctx, "feature");
        std::fs::write(ctx.repo_root.join("shared.txt"), "main version\n").unwrap();
        git(&ctx.repo_root, &["commit", "-am", "main edit"]);
        std::fs::write(path.join("shared.txt"), "feature version\n").unwrap();
        git(&path, &["commit", "-am", "feature edit"]);

        let outcome = merge(&ctx, "feature", "main", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Conflicted { .. }));

        // Resolve the conflict, stage it, and let merge_continue commit it.
        std::fs::write(path.join("shared.txt"), "resolved\n").unwrap();
        git(&path, &["add", "shared.txt"]);
        git::merge_continue(&path).unwrap();
        assert!(!git::is_merging(&path));
        // The merge commit git prepared is kept, recording both parents.
        assert!(
            git::head_subject(&path)
                .unwrap()
                .contains("Merge branch 'main'")
        );
    }

    #[test]
    fn merge_rejects_missing_source_and_self_merge() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feature");
        assert!(merge(&ctx, "feature", "nope", false).is_err());
        assert!(merge(&ctx, "feature", "feature", false).is_err());
    }

    #[test]
    fn update_merges_default_branch_and_refuses_on_default() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(ctx.repo_root.join("new.txt"), "x\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "advance main"]);

        let outcome = update(&ctx, "feature").unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        assert!(path.join("new.txt").exists());

        // The main worktree has the default branch itself: nothing to update.
        assert!(update(&ctx, "main").is_err());
    }

    #[test]
    fn parses_multiple_hunks_interleaved_with_plain_text() {
        let text = "line1\n\
                     <<<<<<< HEAD\n\
                     ours1\n\
                     =======\n\
                     theirs1\n\
                     >>>>>>> feature\n\
                     middle\n\
                     <<<<<<< HEAD\n\
                     ours2\n\
                     =======\n\
                     theirs2\n\
                     >>>>>>> feature\n\
                     end\n";
        let segments = conflict::parse(text);
        assert_eq!(
            segments,
            vec![
                conflict::ConflictSegment::Plain("line1\n".to_string()),
                conflict::ConflictSegment::Hunk {
                    ours: "ours1\n".to_string(),
                    theirs: "theirs1\n".to_string(),
                    base: None,
                },
                conflict::ConflictSegment::Plain("middle\n".to_string()),
                conflict::ConflictSegment::Hunk {
                    ours: "ours2\n".to_string(),
                    theirs: "theirs2\n".to_string(),
                    base: None,
                },
                conflict::ConflictSegment::Plain("end\n".to_string()),
            ]
        );
    }

    #[test]
    fn parses_diff3_hunk_with_base_section() {
        let text = "<<<<<<< HEAD\n\
                     ours\n\
                     ||||||| merged common ancestors\n\
                     base\n\
                     =======\n\
                     theirs\n\
                     >>>>>>> feature\n";
        let segments = conflict::parse(text);
        assert_eq!(
            segments,
            vec![conflict::ConflictSegment::Hunk {
                ours: "ours\n".to_string(),
                theirs: "theirs\n".to_string(),
                base: Some("base\n".to_string()),
            }]
        );
    }

    #[test]
    fn render_applies_each_resolution_action() {
        let segments = vec![conflict::ConflictSegment::Hunk {
            ours: "O\n".to_string(),
            theirs: "T\n".to_string(),
            base: None,
        }];
        assert_eq!(
            conflict::render(&segments, &[conflict::ResolutionAction::KeepOurs]),
            "O\n"
        );
        assert_eq!(
            conflict::render(&segments, &[conflict::ResolutionAction::KeepTheirs]),
            "T\n"
        );
        assert_eq!(
            conflict::render(&segments, &[conflict::ResolutionAction::KeepBoth]),
            "O\nT\n"
        );
        assert_eq!(
            conflict::render(&segments, &[conflict::ResolutionAction::KeepBothReversed]),
            "T\nO\n"
        );
        assert_eq!(
            conflict::render(
                &segments,
                &[conflict::ResolutionAction::Manual("X\n".to_string())]
            ),
            "X\n"
        );
    }

    #[test]
    fn round_trips_a_file_with_no_conflicts() {
        let text = "no markers here\njust plain lines\n";
        let segments = conflict::parse(text);
        assert_eq!(conflict::render(&segments, &[]), text);
    }

    /// Sets up a real conflicted merge: `feature` and `main` each edit the
    /// same line of `shared.txt`. Returns the ctx and the target worktree's
    /// path, already mid-merge.
    fn make_conflicted_merge(ctx: &Ctx) -> PathBuf {
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(ctx, "feature");
        std::fs::write(ctx.repo_root.join("shared.txt"), "main version\n").unwrap();
        git(&ctx.repo_root, &["commit", "-am", "main edit"]);
        std::fs::write(path.join("shared.txt"), "feature version\n").unwrap();
        git(&path, &["commit", "-am", "feature edit"]);
        let outcome = merge(ctx, "feature", "main", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Conflicted { .. }));
        path
    }

    #[test]
    fn read_conflict_write_resolution_and_complete_merge_roundtrip() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_conflicted_merge(&ctx);

        assert_eq!(
            list_conflicts(&ctx, "feature").unwrap(),
            vec!["shared.txt".to_string()]
        );

        let file = read_conflict(&ctx, "feature", "shared.txt").unwrap();
        assert_eq!(file.path, "shared.txt");
        assert_eq!(
            file.segments,
            vec![conflict::ConflictSegment::Hunk {
                ours: "feature version\n".to_string(),
                theirs: "main version\n".to_string(),
                base: None,
            }]
        );
        // Default markers label ours as HEAD and theirs as the merged branch.
        assert_eq!(file.ours_label, "HEAD");
        assert_eq!(file.theirs_label, "main");

        // Resolve by keeping both, in order, then finish the merge.
        let resolved = conflict::render(&file.segments, &[conflict::ResolutionAction::KeepBoth]);
        write_resolution(&ctx, "feature", "shared.txt", &resolved).unwrap();
        assert!(git::conflicted_files(&path).unwrap().is_empty());

        let result = complete_resolution(
            &ctx,
            "feature",
            ResolveKind::Merge,
            Some("merge main into feature"),
        )
        .unwrap();
        assert_eq!(result.target, "feature");
        assert!(result.commit.as_deref().is_some_and(|c| !c.is_empty()));
        assert!(!git::is_merging(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\nmain version\n"
        );
        assert_eq!(git::head_subject(&path).unwrap(), "merge main into feature");
    }

    #[test]
    fn checkout_ours_and_theirs_resolve_whole_file() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_conflicted_merge(&ctx);

        checkout_ours(&ctx, "feature", "shared.txt").unwrap();
        assert!(git::conflicted_files(&path).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\n"
        );
        complete_resolution(&ctx, "feature", ResolveKind::Merge, None).unwrap();
        assert!(!git::is_merging(&path));
    }

    #[test]
    fn complete_merge_refuses_with_unresolved_conflicts() {
        let (_tmp, ctx) = temp_ctx();
        make_conflicted_merge(&ctx);
        let err = complete_resolution(&ctx, "feature", ResolveKind::Merge, None).unwrap_err();
        assert!(err.to_string().contains("shared.txt"));
    }

    #[test]
    fn abort_merge_recovers_a_conflicted_worktree() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_conflicted_merge(&ctx);

        abort_resolution(&ctx, "feature", ResolveKind::Merge).unwrap();
        assert!(!git::is_merging(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\n"
        );
    }

    /// Sets up a worktree whose branch and the default branch each changed the
    /// same line, so cherry-picking main's commit onto the feature branch
    /// conflicts. Returns the worktree path and the conflicting commit's hash.
    fn make_conflicting_cherry_pick(ctx: &Ctx) -> (PathBuf, String) {
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(ctx, "feature");
        // A commit on main that edits shared.txt (the one we cherry-pick).
        std::fs::write(ctx.repo_root.join("shared.txt"), "main version\n").unwrap();
        git(&ctx.repo_root, &["commit", "-am", "main edit"]);
        let hash = git::short_hash(&ctx.repo_root).unwrap();
        // A divergent edit on the feature branch to the same line.
        std::fs::write(path.join("shared.txt"), "feature version\n").unwrap();
        git(&path, &["commit", "-am", "feature edit"]);
        (path, hash)
    }

    #[test]
    fn cherry_pick_conflict_leaves_tree_mid_pick_and_continues() {
        let (_tmp, ctx) = temp_ctx();
        let (path, hash) = make_conflicting_cherry_pick(&ctx);

        let outcome = cherry_pick(&ctx, "feature", &[hash], false).unwrap();
        let CherryPickOutcome::Conflicted { files, .. } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        assert_eq!(files, vec!["shared.txt".to_string()]);
        // The sequence is left in progress for the resolver.
        assert!(git::is_cherry_picking(&path));
        assert_eq!(
            detect_resolve_kind(&ctx, "feature").unwrap(),
            Some(ResolveKind::CherryPick)
        );

        // Resolve, stage, and continue: the pick commits and the tree is clean.
        std::fs::write(path.join("shared.txt"), "resolved\n").unwrap();
        git(&path, &["add", "shared.txt"]);
        let result = complete_resolution(&ctx, "feature", ResolveKind::CherryPick, None).unwrap();
        assert!(result.commit.is_some());
        assert!(!git::is_cherry_picking(&path));
        assert_eq!(git::head_subject(&path).unwrap(), "main edit");
    }

    #[test]
    fn cherry_pick_abort_recovers_a_conflicted_worktree() {
        let (_tmp, ctx) = temp_ctx();
        let (path, hash) = make_conflicting_cherry_pick(&ctx);

        cherry_pick(&ctx, "feature", &[hash], false).unwrap();
        abort_resolution(&ctx, "feature", ResolveKind::CherryPick).unwrap();
        assert!(!git::is_cherry_picking(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\n"
        );
    }

    #[test]
    fn stash_pop_conflict_lists_files_then_completes_by_dropping_stash() {
        let (_tmp, ctx) = temp_ctx();
        // Commit a base file, then diverge the committed and stashed versions of
        // the same line so re-applying the stash conflicts.
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(&ctx, "feature");

        // Stash a change to shared.txt, then commit a different change to the
        // same line so the stash can't reapply cleanly.
        std::fs::write(path.join("shared.txt"), "stashed version\n").unwrap();
        stash_push(&ctx, "feature", None).unwrap();
        std::fs::write(path.join("shared.txt"), "committed version\n").unwrap();
        git(&path, &["commit", "-am", "committed edit"]);

        let outcome = stash_pop(&ctx, "feature", None).unwrap();
        let StashPopOutcome::Conflicted { files, index, .. } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        assert_eq!(files, vec!["shared.txt".to_string()]);
        // A stash pop leaves no merge/cherry-pick marker.
        assert!(!git::is_merging(&path));
        assert!(!git::is_cherry_picking(&path));
        assert_eq!(detect_resolve_kind(&ctx, "feature").unwrap(), None);
        // The stash is kept until the resolution completes.
        assert_eq!(git::stash_list(&path).unwrap().len(), 1);

        // Resolve, stage, and complete: no commit, the stash is dropped, and the
        // tree is left clean with the resolved contents.
        std::fs::write(path.join("shared.txt"), "resolved\n").unwrap();
        git(&path, &["add", "shared.txt"]);
        let result =
            complete_resolution(&ctx, "feature", ResolveKind::StashPop { index }, None).unwrap();
        assert!(result.commit.is_none());
        assert!(git::stash_list(&path).unwrap().is_empty());
        assert!(git::conflicted_files(&path).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "resolved\n"
        );
    }

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

    #[test]
    fn ignore_pattern_uses_extension_or_bare_name() {
        assert_eq!(ignore_pattern("src/foo.log"), "*.log");
        assert_eq!(ignore_pattern("build/app.tmp"), "*.tmp");
        // No extension: fall back to the bare file name.
        assert_eq!(ignore_pattern("bin/Makefile"), "Makefile");
        assert_eq!(ignore_pattern(".env"), ".env");
    }

    /// Wires `ctx`'s repo up to a fresh bare "origin" and publishes `main`,
    /// returning the bare repo's path.
    fn with_origin(tmp: &Path, ctx: &Ctx) -> PathBuf {
        let bare = tmp.join("origin.git");
        git(
            tmp,
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(
            &ctx.repo_root,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&ctx.repo_root, &["push", "-u", "origin", "main"]);
        bare
    }

    /// Advances `branch` on the bare remote from an independent clone, so the
    /// repo under test genuinely falls behind its upstream.
    fn advance_remote(tmp: &Path, bare: &Path, branch: &str, message: &str) {
        let clone = tmp.join(format!("clone-{message}"));
        git(
            tmp,
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.email", "t@e.st"]);
        git(&clone, &["config", "user.name", "t"]);
        git(&clone, &["checkout", branch]);
        git(&clone, &["commit", "--allow-empty", "-m", message]);
        git(&clone, &["push", "origin", branch]);
    }

    #[test]
    fn branch_pull_requires_an_upstream() {
        let (_tmp, ctx) = temp_ctx();
        let err = branch_pull(&ctx, "main").unwrap_err().to_string();
        assert!(err.contains("no upstream"), "unexpected error: {err}");
    }

    #[test]
    fn branch_pull_rejects_an_unknown_branch() {
        let (_tmp, ctx) = temp_ctx();
        let err = branch_pull(&ctx, "nope").unwrap_err().to_string();
        assert!(err.contains("no local branch"), "unexpected error: {err}");
    }

    /// A branch that is behind but checked out nowhere fast-forwards in place,
    /// without a working tree to check it out into.
    #[test]
    fn branch_pull_fast_forwards_a_branch_with_no_worktree() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        // `side` exists locally and on the remote, checked out nowhere here.
        git(&ctx.repo_root, &["branch", "side", "main"]);
        git(&ctx.repo_root, &["push", "-u", "origin", "side"]);
        advance_remote(tmp.path(), &bare, "side", "remote-work");

        let before = git::run(&ctx.repo_root, &["rev-parse", "side"]).unwrap();
        let r = branch_pull(&ctx, "side").unwrap();
        assert_eq!(r.branch, "side");
        assert!(!r.already_up_to_date);
        // Nothing had it checked out, so no worktree is named.
        assert_eq!(r.worktree, None);
        let after = git::run(&ctx.repo_root, &["rev-parse", "side"]).unwrap();
        assert_ne!(before, after, "side should have moved");
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s", "side"]).unwrap(),
            "remote-work"
        );
        // The main worktree is untouched by a pull of some other branch.
        assert_eq!(
            git::run(&ctx.repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "main"
        );

        // Pulling again has nothing to do and says so.
        let r = branch_pull(&ctx, "side").unwrap();
        assert!(r.already_up_to_date);
    }

    /// When the branch *is* checked out, the pull happens in that worktree so
    /// its files move with the branch, and the result names it.
    #[test]
    fn branch_pull_pulls_in_the_worktree_holding_the_branch() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        advance_remote(tmp.path(), &bare, "main", "remote-work");

        let r = branch_pull(&ctx, "main").unwrap();
        assert!(!r.already_up_to_date);
        assert_eq!(r.worktree.as_deref(), Some("main"));
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s", "main"]).unwrap(),
            "remote-work"
        );
    }

    /// A diverged branch must fail rather than quietly merging: fast-forward is
    /// the whole contract of this operation.
    #[test]
    fn branch_pull_refuses_to_merge_a_diverged_branch() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        git(&ctx.repo_root, &["branch", "side", "main"]);
        git(&ctx.repo_root, &["push", "-u", "origin", "side"]);
        advance_remote(tmp.path(), &bare, "side", "remote-work");
        // Put a different commit on the local side, so the two histories fork.
        git(&ctx.repo_root, &["checkout", "side"]);
        git(&ctx.repo_root, &["commit", "--allow-empty", "-m", "local"]);
        git(&ctx.repo_root, &["checkout", "main"]);

        assert!(
            branch_pull(&ctx, "side").is_err(),
            "a diverged branch must not fast-forward"
        );
        // The local commit survives the refusal.
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s", "side"]).unwrap(),
            "local"
        );
    }
}
