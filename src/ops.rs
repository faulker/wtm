//! Core worktree operations shared by the CLI, TUI, and MCP server.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

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

/// Pops a stash entry (default most recent) in the worktree named `name`.
pub fn stash_pop(ctx: &Ctx, name: &str, index: Option<u32>) -> Result<StashResult> {
    stash_action(ctx, name, "pop", index, git::stash_pop)
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

/// Switches the worktree named `name` to check out the existing local branch
/// `branch`. Refuses when the branch is already checked out in another worktree
/// (git forbids this) or is already the worktree's current branch.
pub fn switch_branch(ctx: &Ctx, name: &str, branch: &str) -> Result<SwitchResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    if info.branch.as_deref() == Some(branch) {
        bail!("worktree '{}' already has '{branch}' checked out", info.name);
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
        git(&ctx.repo_root, &["worktree", "lock", path.to_str().unwrap()]);

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
}
