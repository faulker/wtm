//! Core worktree operations shared by the CLI, TUI, and MCP server.

use std::collections::HashSet;
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
    /// True when this worktree's branch is fully contained in the repo's
    /// default branch (every commit already merged), so the worktree is safe to
    /// clean up. False for the main worktree, a detached HEAD, the default
    /// branch itself, or a branch with commits not yet on the mainline.
    pub merged: bool,
    /// Parent used for `same`/`changed`/`outdated`: recorded `[created_from]`
    /// when that ref resolves, else the repo default branch name (even when
    /// status was computed via merge-base against that parent). `null` for the
    /// main worktree, detached HEADs, the default branch itself, or when no
    /// parent could be resolved at all.
    pub created_from: Option<String>,
    /// True when the branch has commits not reachable from its comparison base
    /// (unique local work vs that base). Always false for the main worktree,
    /// when the base is unknown/missing, or when the tip still matches the base.
    pub changed_from_base: bool,
    /// True when the comparison **tip** has commits not in this branch (base
    /// moved ahead; the worktree is out of date). Always false for the main
    /// worktree, when the base is unknown/missing, or when only a merge-base
    /// SHA was available (a merge-base is always an ancestor, so it cannot
    /// signal "outdated").
    pub behind_base: bool,
    /// Number of files in an unmerged (conflict) state. Counted from the same
    /// status listing as `dirty`, so it costs nothing extra.
    pub conflicted: usize,
    /// The merge/rebase/cherry-pick this worktree is stopped in the middle of,
    /// if any, so the UI can flag it and offer to resume. `null` when the
    /// worktree is not mid-operation. A conflicted stash pop leaves no marker
    /// on disk and so cannot be detected here.
    pub in_progress: Option<ResolveKind>,
}

impl WorktreeInfo {
    /// Status label for `open_command` templates: `"merged"`, `"ahead"`,
    /// `"behind"`, or `""` when none of those apply. Merged wins over
    /// ahead/behind so a merged branch that is also behind upstream still
    /// expands as `merged`.
    pub fn open_status(&self) -> &'static str {
        if self.merged {
            return "merged";
        }
        match &self.ahead_behind {
            Some(ab) if ab.ahead > 0 => "ahead",
            Some(ab) if ab.behind > 0 => "behind",
            _ => "",
        }
    }

    /// Plain-language FLAGS labels for TUI/CLI (upstream sync, then base
    /// status, then cleanup/lock). Same vocabulary as `branch_flag_labels`.
    pub fn flag_labels(&self) -> Vec<&'static str> {
        flag_labels(
            self.ahead_behind.as_ref().map(|ab| (ab.ahead, ab.behind)),
            self.changed_from_base,
            self.behind_base,
            /* show_same */ self.created_from.is_some() && !self.is_main,
            self.merged,
            self.locked,
        )
    }
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
    // The mainline every worktree's branch is measured against for the "merged"
    // flag, plus its first-parent trunk (computed once). Best-effort: a repo with
    // no resolvable default just leaves the flag unset.
    let default = git::default_branch(&ctx.repo_root).ok();
    let trunk = match &default {
        Some(d) => git::first_parent_commits(&ctx.repo_root, d).unwrap_or_default(),
        None => HashSet::new(),
    };
    // Creation bases recorded by `create` in `.wtm.toml` `[created_from]`.
    let created_from_map = crate::config::load_created_from(&ctx.repo_root).unwrap_or_default();
    let mut infos = Vec::with_capacity(wts.len());
    for wt in wts {
        if wt.is_bare {
            continue;
        }
        let is_main = wt.path == ctx.repo_root;
        // A worktree directory can disappear out from under git (deleted by
        // hand); report it rather than failing the whole listing.
        let exists = wt.path.exists();
        let (dirty, conflicted, ahead_behind, in_progress) = if exists {
            let status = git::status(&wt.path)?;
            let conflicted = status
                .iter()
                .filter(|e| git::is_conflict_code(&e.code))
                .count();
            (
                status.len(),
                conflicted,
                git::ahead_behind(&wt.path)?,
                detect_resolve_kind_in(&wt.path),
            )
        } else {
            (0, 0, None, None)
        };
        // Whether this worktree's branch has been merged into the default
        // branch. Skip the main worktree and the default branch itself (nothing
        // to merge into), and detached HEADs (no branch tip).
        let merged = match (&default, &wt.branch) {
            (Some(default), Some(branch))
                if !is_main && branch != default && git::branch_exists(&ctx.repo_root, branch) =>
            {
                branch_merged_into(&ctx.repo_root, default, branch, &trunk)?
            }
            _ => false,
        };
        let (created_from, changed_from_base, behind_base) = match &wt.branch {
            Some(branch) if !is_main => {
                let status = resolve_base_status(
                    &ctx.repo_root,
                    branch,
                    created_from_map.get(branch).map(String::as_str),
                    default.as_deref(),
                );
                (status.label, status.changed, status.behind)
            }
            _ => (None, false, false),
        };
        infos.push(WorktreeInfo {
            name: worktree_name(&wt.branch, &wt.path),
            branch: wt.branch,
            path: wt.path.to_string_lossy().to_string(),
            is_main,
            dirty,
            ahead_behind,
            locked: wt.is_locked,
            merged,
            created_from,
            changed_from_base,
            behind_base,
            conflicted,
            in_progress,
        });
    }
    Ok(infos)
}

/// How a branch tip was compared for `same`/`changed`/`outdated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseCompareKind {
    /// Named tip (creation base or default branch): both changed and outdated.
    Tip,
    /// Merge-base SHA only: changed is meaningful; outdated is always false.
    MergeBase,
}

/// Resolved comparison base for a branch's status flags.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BaseStatus {
    /// Parent label shown as `created_from` (named tip or merge-base parent).
    label: Option<String>,
    changed: bool,
    behind: bool,
}

/// Resolves the comparison base for `branch`:
/// 1. recorded `created_from` when that ref still exists
/// 2. else the repo default branch tip (when it exists and is not `branch`)
/// 3. else `git merge-base` against the default / `origin/HEAD`
///
/// Returns an empty status when nothing is comparable (default branch itself,
/// missing refs, etc.).
fn resolve_base_status(
    repo_root: &Path,
    branch: &str,
    recorded: Option<&str>,
    default: Option<&str>,
) -> BaseStatus {
    if !git::branch_exists(repo_root, branch) {
        return BaseStatus {
            label: None,
            changed: false,
            behind: false,
        };
    }
    // Prefer a recorded creation base that still resolves.
    if let Some(base) = recorded.filter(|b| git::ref_exists(repo_root, b)) {
        return base_status_vs(repo_root, base, branch, BaseCompareKind::Tip);
    }
    // Fall back to the default branch tip when it is a different, resolvable ref.
    if let Some(def) = default.filter(|d| *d != branch) {
        if git::ref_exists(repo_root, def) {
            return base_status_vs(repo_root, def, branch, BaseCompareKind::Tip);
        }
        // Local default missing: try the remote-tracking tip of the same name.
        let remote_tip = format!("origin/{def}");
        if git::ref_exists(repo_root, &remote_tip) {
            return base_status_vs(repo_root, &remote_tip, branch, BaseCompareKind::Tip)
                .with_label(def.to_string());
        }
    }
    // Last resort: merge-base against a resolvable parent (changed only).
    // Tip paths above already covered local/remote default tips; this runs when
    // those tips are missing but e.g. origin/HEAD still resolves.
    let mut merge_parents: Vec<(String, String)> = Vec::new();
    if let Some(def) = default.filter(|d| *d != branch) {
        if git::ref_exists(repo_root, def) {
            merge_parents.push((def.to_string(), def.to_string()));
        } else {
            let remote = format!("origin/{def}");
            if git::ref_exists(repo_root, &remote) {
                merge_parents.push((remote, def.to_string()));
            }
        }
    }
    if git::ref_exists(repo_root, "origin/HEAD") {
        let label = default
            .filter(|d| *d != branch)
            .unwrap_or("origin/HEAD")
            .to_string();
        merge_parents.push(("origin/HEAD".to_string(), label));
    }
    for (parent, label) in merge_parents {
        if let Ok(mb) = git::merge_base(repo_root, branch, &parent)
            && !mb.is_empty()
        {
            return base_status_vs(repo_root, &mb, branch, BaseCompareKind::MergeBase)
                .with_label(label);
        }
    }
    BaseStatus {
        label: None,
        changed: false,
        behind: false,
    }
}

impl BaseStatus {
    fn with_label(mut self, label: String) -> Self {
        self.label = Some(label);
        self
    }
}

/// Compares `branch` to `base`: unique commits (`changed`) and, for tip bases,
/// whether the base has moved ahead (`behind`). Merge-base comparisons never
/// set `behind`. Missing/unresolvable bases degrade to empty flags.
fn base_status_vs(repo_root: &Path, base: &str, branch: &str, kind: BaseCompareKind) -> BaseStatus {
    if !git::ref_exists(repo_root, base) {
        return BaseStatus {
            label: None,
            changed: false,
            behind: false,
        };
    }
    let changed = git::commits_ahead_of(repo_root, base, branch).unwrap_or(0) > 0;
    let behind = match kind {
        BaseCompareKind::Tip => git::commits_ahead_of(repo_root, branch, base).unwrap_or(0) > 0,
        BaseCompareKind::MergeBase => false,
    };
    BaseStatus {
        label: Some(base.to_string()),
        changed,
        behind,
    }
}

/// Shared FLAGS vocabulary for worktrees and branches.
fn flag_labels(
    upstream: Option<(u32, u32)>,
    changed_from_base: bool,
    behind_base: bool,
    show_same: bool,
    merged: bool,
    locked: bool,
) -> Vec<&'static str> {
    let mut parts = Vec::new();
    if let Some((ahead, behind)) = upstream {
        if ahead > 0 {
            parts.push("unpushed");
        }
        if behind > 0 {
            parts.push("behind");
        }
    }
    if changed_from_base {
        parts.push("changed");
    } else if show_same && !behind_base {
        parts.push("same");
    }
    if behind_base {
        parts.push("outdated");
    }
    if merged {
        parts.push("merged");
    }
    if locked {
        parts.push("locked");
    }
    parts
}

/// Resolves the base ref to persist at create time. `HEAD` becomes the current
/// branch name of the main worktree when attached, so later ahead/behind
/// checks track that branch as it moves rather than a moving HEAD.
fn resolve_creation_base(repo_root: &Path, base: &str) -> String {
    if base != "HEAD" {
        return base.to_string();
    }
    match git::head_branch(repo_root) {
        Ok(Some(branch)) => branch,
        _ => git::rev_parse(repo_root, "HEAD").unwrap_or_else(|_| "HEAD".to_string()),
    }
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

    // Persist the creation base so `list` can flag unique commits / out-of-date
    // vs the branch this worktree was spun from. Only for newly created
    // branches (checking out an existing branch has no known create base).
    if let Some(raw_base) = &base {
        let stored = resolve_creation_base(&ctx.repo_root, raw_base);
        // Best-effort: a write failure must not undo a successful worktree add.
        let _ = crate::config::set_created_from(&ctx.repo_root, branch, &stored);
    }

    let mut setup = Vec::new();
    for file in &ctx.config.setup.copy {
        // Report each copy as it happens; otherwise the progress log sits
        // empty until the first setup command runs and the UI looks stalled.
        progress(&format!("copying: {}", file.display()));
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
    let registered = git::list_worktrees(&ctx.repo_root)?
        .into_iter()
        .find(|w| std::fs::canonicalize(&w.path).ok() == canon && canon.is_some());
    if let Some(reg) = &registered {
        // Unlock first so the subsequent remove/prune can reclaim a locked
        // worktree; harmless (and ignored) when it was not locked.
        let _ = git::worktree_unlock(&ctx.repo_root, &reg.path);
        // Best effort: if git still refuses we fall back to deleting the
        // directory and pruning below.
        let _ = git::worktree_remove(&ctx.repo_root, &reg.path, true);
        if let Some(branch) = &reg.branch {
            let _ = crate::config::unset_created_from(&ctx.repo_root, branch);
        }
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
    if let Some(branch) = &info.branch {
        let _ = crate::config::unset_created_from(&ctx.repo_root, branch);
    }
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
    if let Some(branch) = &info.branch {
        let _ = crate::config::unset_created_from(&ctx.repo_root, branch);
    }
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

/// Files changed by commit `hash`, viewed from the worktree named `name`.
pub fn commit_files(ctx: &Ctx, name: &str, hash: &str) -> Result<Vec<StatusEntry>> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::commit_files(Path::new(&info.path), hash).map_err(Into::into)
}

/// Unified diff of a single `path` as changed by commit `hash`, viewed from the
/// worktree named `name`.
pub fn commit_file_diff(ctx: &Ctx, name: &str, hash: &str, path: &str) -> Result<String> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::commit_file_diff(Path::new(&info.path), hash, path).map_err(Into::into)
}

/// Files changed by stash entry `index`, viewed from the worktree named `name`
/// (stashes are repo-global; the worktree only supplies a git directory).
pub fn stash_files(ctx: &Ctx, name: &str, index: u32) -> Result<Vec<StatusEntry>> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::stash_files(Path::new(&info.path), index).map_err(Into::into)
}

/// Unified diff of a single `path` as changed by stash entry `index`.
pub fn stash_file_diff(ctx: &Ctx, name: &str, index: u32, path: &str) -> Result<String> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::stash_file_diff(Path::new(&info.path), index, path).map_err(Into::into)
}

/// Discards uncommitted changes to `path` in the worktree named `name`,
/// restoring it to HEAD (or removing it if it was untracked).
pub fn revert_file(ctx: &Ctx, name: &str, path: &str, untracked: bool) -> Result<()> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::revert_file(Path::new(&info.path), path, untracked).map_err(Into::into)
}

/// Deletes `path` from the worktree named `name`, removing it from disk (and
/// staging the removal for tracked files).
pub fn delete_file(ctx: &Ctx, name: &str, path: &str, untracked: bool) -> Result<()> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::delete_file(Path::new(&info.path), path, untracked).map_err(Into::into)
}

/// Discards every uncommitted change in the worktree named `name`: tracked
/// changes reset to HEAD, untracked files removed.
pub fn discard_all_changes(ctx: &Ctx, name: &str) -> Result<()> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    git::discard_all(Path::new(&info.path)).map_err(Into::into)
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

/// Result of `move_changes`.
#[derive(Debug, Clone, Serialize)]
pub struct MoveChangesResult {
    pub from: String,
    pub to: String,
    /// Number of files whose changes were moved.
    pub files: usize,
}

/// Result of `pull`.
#[derive(Debug, Clone, Serialize)]
pub struct PullResult {
    pub name: String,
    pub already_up_to_date: bool,
    /// Ahead/behind upstream after the pull.
    pub ahead_behind: Option<AheadBehind>,
    /// Files left in conflict when a `--rebase` pull stopped on one. Empty on a
    /// clean pull. The worktree is left mid-rebase so these can be resolved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicted: Vec<String>,
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
    /// Whether the branch's work has been merged into the repo's default branch.
    pub merged: bool,
    /// `Some("origin/feature")` when this branch exists only on a remote, with
    /// no local branch yet; `None` for a normal local branch.
    pub remote: Option<String>,
    /// Effective comparison parent for base flags (see `WorktreeInfo::created_from`).
    pub created_from: Option<String>,
    /// True when this branch has commits not in its comparison base.
    pub changed_from_base: bool,
    /// True when the comparison tip has moved ahead of this branch.
    pub behind_base: bool,
}

impl BranchListItem {
    /// Plain-language FLAGS labels (same vocabulary as worktrees).
    pub fn flag_labels(&self) -> Vec<&'static str> {
        let upstream = self.upstream.as_ref().map(|_| (self.ahead, self.behind));
        flag_labels(
            upstream,
            self.changed_from_base,
            self.behind_base,
            /* show_same */ self.created_from.is_some() && self.remote.is_none(),
            self.merged,
            false,
        )
    }
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

/// Result of `branch upstream`: what the branch tracks now and what it tracked
/// before, so a caller can report the change rather than just the new state.
#[derive(Debug, Clone, Serialize)]
pub struct BranchUpstreamResult {
    pub name: String,
    /// The remote-tracking ref it now follows; `None` when tracking was removed.
    pub upstream: Option<String>,
    /// What it tracked before, if anything.
    pub previous: Option<String>,
}

/// Result of `rename` (a worktree): the branch was renamed and the directory
/// moved to match.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeRenameResult {
    pub old_name: String,
    pub new_name: String,
    pub old_path: String,
    pub new_path: String,
    /// Whether a branch was renamed (false for a detached-HEAD worktree, where
    /// only the directory moved).
    pub renamed_branch: bool,
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
    /// The target branch already contained the source (merge), or the default
    /// branch was already at its upstream (`update` when the target is on the
    /// default branch); nothing changed.
    UpToDate,
    /// The merge completed; `commit` is the short hash of the target's new HEAD.
    Clean { commit: String },
    /// `update` fast-forwarded the default branch in place because the target
    /// already had it checked out. `commit` is the short hash of the new HEAD.
    /// Distinct from [`Self::Clean`] so CLI/TUI can say "fast-forwarded" rather
    /// than "merged".
    FastForwarded { commit: String },
    /// The merge stopped on conflicts; the target worktree is left mid-merge
    /// so the listed files can be resolved there.
    Conflicted { files: Vec<String> },
}

/// Which in-progress operation a set of conflicts belongs to, so the resolver's
/// "complete"/"abort" can dispatch correctly. Merge and cherry-pick leave a
/// marker ref in the repo (MERGE_HEAD / CHERRY_PICK_HEAD) and a rebase leaves a
/// state directory, so all three can be detected after the fact and finish by
/// continuing that sequence; a stash pop leaves no marker at all, so finishing
/// means dropping the applied stash entry (no new commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolveKind {
    Merge,
    Rebase,
    CherryPick,
    StashPop {
        /// Stash entry to drop on completion (the one that was popped).
        index: Option<u32>,
    },
}

impl ResolveKind {
    /// How the operation is named in messages, e.g. "no rebase in progress".
    pub fn label(&self) -> &'static str {
        match self {
            ResolveKind::Merge => "merge",
            ResolveKind::Rebase => "rebase",
            ResolveKind::CherryPick => "cherry-pick",
            ResolveKind::StashPop { .. } => "stash pop",
        }
    }

    /// True when the operation replays your commits on top of someone else's
    /// work, which swaps what git calls "ours" and "theirs": during a rebase
    /// "ours" is the branch being rebased *onto* and "theirs" is your own
    /// commit being replayed, the opposite of a merge. Callers that explain the
    /// two sides to a human must account for this or they mislead.
    pub fn sides_are_swapped(&self) -> bool {
        matches!(self, ResolveKind::Rebase)
    }
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
/// by default, or only `paths` when given. `body` becomes the commit body
/// below the subject line. Refuses when nothing is staged.
pub fn commit(
    ctx: &Ctx,
    name: &str,
    message: &str,
    body: Option<&str>,
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
    let body = body.map(str::trim).filter(|b| !b.is_empty());
    git::commit(dir, message, body)?;
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

/// Moves uncommitted changes (including untracked files) from the worktree
/// named `from` into the worktree named `to`: stashes everything in `from`,
/// applies it in `to`, then drops the stash. Refuses when `from` has nothing
/// to move or `to` isn't clean, so a move never has to untangle the
/// destination's own edits from the incoming ones. If applying at the
/// destination fails (e.g. the changes don't apply there), the stash is
/// re-applied to `from` instead of being left stranded.
pub fn move_changes(ctx: &Ctx, from: &str, to: &str) -> Result<MoveChangesResult> {
    let from_info = find(ctx, from)?.ok_or_else(|| not_found(ctx, from))?;
    let to_info = find(ctx, to)?.ok_or_else(|| not_found(ctx, to))?;
    if from_info.path == to_info.path {
        bail!("'{from}' and '{to}' are the same worktree");
    }
    let from_dir = Path::new(&from_info.path);
    let to_dir = Path::new(&to_info.path);
    let files = git::status(from_dir)?.len();
    if files == 0 {
        bail!(
            "worktree '{}' has no uncommitted changes to move",
            from_info.name
        );
    }
    if !git::status(to_dir)?.is_empty() {
        bail!(
            "worktree '{}' has uncommitted changes of its own; commit or stash them first",
            to_info.name
        );
    }
    git::stash_push(from_dir, Some(&format!("moved to '{}'", to_info.name)))?;
    if let Err(e) = git::stash_apply(to_dir, None) {
        // Applying at the destination failed; restore the change to where it
        // came from rather than leaving it stranded only in the stash list.
        git::stash_pop(from_dir, None).ok();
        return Err(e).context(format!(
            "could not apply changes into '{}'; restored them to '{}'",
            to_info.name, from_info.name
        ));
    }
    git::stash_drop(from_dir, None)?;
    Ok(MoveChangesResult {
        from: from_info.name,
        to: to_info.name,
        files,
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
    let output = match git::pull(dir, rebase) {
        Ok(o) => o,
        // A refused fast-forward means the branch has diverged; point at the
        // rebase escape hatch instead of leaving the raw git error alone.
        Err(e) if !rebase && git::is_non_fast_forward(&e.to_string()) => {
            return Err(anyhow!(e).context(format!(
                "'{}' has diverged from its upstream, so a fast-forward pull isn't \
                 possible; retry with `wtm pull {} --rebase` to rebase the local \
                 commits onto the upstream",
                info.name, info.name
            )));
        }
        // A rebasing pull that stops on a conflict exits non-zero but leaves a
        // rebase in progress. That is a resolvable state, not a failure, so
        // report it as one rather than stranding the worktree behind an error.
        Err(e) => {
            let conflicted = git::conflicted_files(dir).unwrap_or_default();
            if rebase && git::is_rebasing(dir) && !conflicted.is_empty() {
                return Ok(PullResult {
                    name: info.name,
                    already_up_to_date: false,
                    ahead_behind: git::ahead_behind(dir).ok().flatten(),
                    conflicted,
                });
            }
            return Err(e.into());
        }
    };
    let already_up_to_date = output.contains("Already up to date");
    let ahead_behind = git::ahead_behind(dir)?;
    Ok(PullResult {
        name: info.name,
        already_up_to_date,
        ahead_behind,
        conflicted: Vec::new(),
    })
}

/// Ahead/behind counts of local branch `branch` vs its upstream, without
/// requiring it to be checked out anywhere. `None` when `branch` isn't a
/// local branch or has no upstream configured.
pub fn branch_ahead_behind(ctx: &Ctx, branch: &str) -> Result<Option<git::AheadBehind>> {
    if !git::branch_exists(&ctx.repo_root, branch) {
        return Ok(None);
    }
    git::branch_ahead_behind(&ctx.repo_root, branch).map_err(Into::into)
}

/// Fast-forwards local branch `branch` to match its upstream: pulls in place
/// if it's checked out in a worktree, otherwise moves the ref directly via
/// fetch. Only safe to call when the branch has no local commits ahead of
/// its upstream.
pub fn update_branch(ctx: &Ctx, branch: &str) -> Result<()> {
    match find(ctx, branch)? {
        Some(info) => {
            pull(ctx, &info.name, false)?;
        }
        None => git::fetch_branch_ff(&ctx.repo_root, branch)?,
    }
    Ok(())
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
    let local_details = git::ref_details(&ctx.repo_root, "refs/heads")?;
    let remote_details = git::ref_details(&ctx.repo_root, "refs/remotes")?;
    let worktrees = git::list_worktrees(&ctx.repo_root)?;
    // The mainline every branch's "merged" flag is measured against, plus its
    // first-parent trunk (computed once). Best-effort: a repo with no resolvable
    // default leaves every branch unflagged.
    let default = git::default_branch(&ctx.repo_root).ok();
    let trunk = match &default {
        Some(d) => git::first_parent_commits(&ctx.repo_root, d).unwrap_or_default(),
        None => HashSet::new(),
    };
    let created_from_map = crate::config::load_created_from(&ctx.repo_root).unwrap_or_default();
    let local_names: HashSet<String> = local_details.iter().map(|d| d.name.clone()).collect();
    let mut branches = Vec::with_capacity(local_details.len());
    for d in local_details {
        let checked_out_path = worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(&d.name))
            .map(|w| w.path.to_string_lossy().to_string());
        let merged = match &default {
            Some(default) => branch_merged_into(&ctx.repo_root, default, &d.name, &trunk)?,
            None => false,
        };
        // Base flags for local branches only; skip the default branch itself.
        let base = resolve_base_status(
            &ctx.repo_root,
            &d.name,
            created_from_map.get(&d.name).map(String::as_str),
            default.as_deref(),
        );
        branches.push(BranchListItem {
            name: d.name,
            checked_out_path,
            upstream: d.upstream,
            ahead: d.ahead,
            behind: d.behind,
            subject: d.subject,
            date: d.date,
            merged,
            remote: None,
            created_from: base.label,
            changed_from_base: base.changed,
            behind_base: base.behind,
        });
    }
    // Remote-tracking refs whose short name (after stripping `<remote>/`) has
    // no matching local branch are branches that exist only on a remote —
    // surface them as their own rows rather than leaving them invisible.
    // Base-relative flags stay off for remote-only rows (noisy / no local tip).
    for d in remote_details {
        if d.name.ends_with("/HEAD") {
            continue;
        }
        let Some(short) = git::remote_short_name(&d.name) else {
            continue;
        };
        if local_names.contains(short) {
            continue;
        }
        let merged = match &default {
            Some(default) => branch_merged_into(&ctx.repo_root, default, &d.name, &trunk)?,
            None => false,
        };
        branches.push(BranchListItem {
            name: short.to_string(),
            checked_out_path: None,
            upstream: d.upstream,
            ahead: d.ahead,
            behind: d.behind,
            subject: d.subject,
            date: d.date,
            merged,
            remote: Some(d.name),
            created_from: None,
            changed_from_base: false,
            behind_base: false,
        });
    }
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

/// Points local branch `name` at the remote-tracking ref `upstream`, or drops
/// its tracking entirely when `upstream` is `None`.
///
/// Only local branches have an upstream to change, so a remote-only row is
/// refused rather than silently doing nothing. Clearing tracking on a branch
/// that has none is likewise refused: git errors there anyway, and saying so
/// plainly beats passing its message through.
pub fn branch_set_upstream(
    ctx: &Ctx,
    name: &str,
    upstream: Option<&str>,
) -> Result<BranchUpstreamResult> {
    if !git::branch_exists(&ctx.repo_root, name) {
        bail!("'{name}' is not a local branch, so it has no upstream to change");
    }
    let previous = git::branch_upstream(&ctx.repo_root, name)?
        .map(|(remote, branch)| format!("{remote}/{branch}"));
    match upstream {
        Some(upstream) => {
            let upstream = upstream.trim();
            if upstream.is_empty() {
                bail!("upstream must not be empty");
            }
            git::set_upstream(&ctx.repo_root, name, upstream)?;
            Ok(BranchUpstreamResult {
                name: name.to_string(),
                upstream: Some(upstream.to_string()),
                previous,
            })
        }
        None => {
            if previous.is_none() {
                bail!("branch '{name}' has no upstream to remove");
            }
            git::unset_upstream(&ctx.repo_root, name)?;
            Ok(BranchUpstreamResult {
                name: name.to_string(),
                upstream: None,
                previous,
            })
        }
    }
}

/// Remote-tracking refs a branch can be set to track (`origin/main`, …).
pub fn upstream_candidates(ctx: &Ctx) -> Result<Vec<String>> {
    Ok(git::remote_tracking_refs(&ctx.repo_root)?)
}

/// Renames branch `old` to `new`.
pub fn branch_rename(ctx: &Ctx, old: &str, new: &str) -> Result<BranchRenameResult> {
    git::branch_rename(&ctx.repo_root, old, new)?;
    Ok(BranchRenameResult {
        old: old.to_string(),
        new: new.to_string(),
    })
}

/// Renames the worktree addressed by `name` to `new_name`: renames its branch
/// (when it is on one) and moves its directory to a sibling folder named after
/// `new_name`, so the worktree stays addressable by the new name. Refuses on the
/// main worktree, which is the repository itself.
pub fn rename_worktree(ctx: &Ctx, name: &str, new_name: &str) -> Result<WorktreeRenameResult> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        bail!("new name must not be empty");
    }
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    if info.is_main {
        bail!("cannot rename the main worktree");
    }
    let old_path = PathBuf::from(&info.path);
    let new_path = old_path
        .parent()
        .map(|p| p.join(sanitize_dir_name(new_name)))
        .ok_or_else(|| anyhow!("worktree path has no parent: {}", info.path))?;
    if new_path != old_path && new_path.exists() {
        bail!("target directory already exists: {}", new_path.display());
    }
    // Rename the branch first so it tracks the new name; git updates the
    // checked-out worktree's HEAD as part of `branch -m`.
    let renamed_branch = match &info.branch {
        Some(branch) if branch != new_name => {
            if git::branch_exists(&ctx.repo_root, new_name) {
                bail!("branch '{new_name}' already exists");
            }
            git::branch_rename(&ctx.repo_root, branch, new_name)?;
            let _ = crate::config::rename_created_from(&ctx.repo_root, branch, new_name);
            true
        }
        Some(_) => false,
        None => false,
    };
    // Then move the directory to match, unless it already sits at the target.
    if new_path != old_path {
        git::worktree_move(&ctx.repo_root, &old_path, &new_path)?;
    }
    Ok(WorktreeRenameResult {
        old_name: name.to_string(),
        new_name: new_name.to_string(),
        old_path: info.path,
        new_path: new_path.to_string_lossy().to_string(),
        renamed_branch,
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
pub fn merge(
    ctx: &Ctx,
    target: &str,
    source_branch: &str,
    no_ff: bool,
    autostash: bool,
) -> Result<MergeOutcome> {
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
    match git::merge(dir, source_branch, no_ff, autostash)? {
        git::MergeStatus::AlreadyUpToDate => Ok(MergeOutcome::UpToDate),
        git::MergeStatus::Merged => Ok(MergeOutcome::Clean {
            commit: git::short_hash(dir)?,
        }),
        git::MergeStatus::Conflicted(files) => Ok(MergeOutcome::Conflicted { files }),
    }
}

/// Rebases the worktree named `target` onto `onto`, replaying the worktree's
/// own commits on top of that branch so its history reads as if it had started
/// there. `autostash` sets local changes aside for the duration.
///
/// Shares [`MergeOutcome`] with `merge`, since the shapes are the same. On a
/// conflict the worktree is left mid-rebase (see [`MergeOutcome::Conflicted`])
/// so the listed files can be resolved there, then finished with
/// [`complete_resolution`], [`skip_resolution`], or [`abort_resolution`].
pub fn rebase(ctx: &Ctx, target: &str, onto: &str, autostash: bool) -> Result<MergeOutcome> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    if let Some(kind) = detect_resolve_kind_in(dir) {
        bail!(
            "worktree '{}' already has a {} in progress; finish or abort it first",
            info.name,
            kind.label()
        );
    }
    // Accept a local branch or a remote-tracking ref like "origin/main", so a
    // worktree can be rebased onto an upstream that has no local branch.
    let onto_ref = if git::branch_exists(&ctx.repo_root, onto) {
        onto.to_string()
    } else if let Some(remote_ref) = git::find_remote_ref(dir, onto)? {
        remote_ref
    } else {
        bail!("no local or remote branch named '{onto}'");
    };
    if info.branch.as_deref() == Some(onto_ref.as_str()) {
        bail!(
            "worktree '{}' already has '{onto_ref}' checked out; nothing to rebase",
            info.name
        );
    }
    match git::rebase(dir, &onto_ref, autostash)? {
        git::RebaseStatus::UpToDate => Ok(MergeOutcome::UpToDate),
        git::RebaseStatus::Rebased => Ok(MergeOutcome::Clean {
            commit: git::short_hash(dir)?,
        }),
        git::RebaseStatus::Conflicted(files) => Ok(MergeOutcome::Conflicted { files }),
    }
}

/// Brings the worktree named `target` up to date with the repository's default
/// branch.
///
/// 1. When the default branch has an upstream, refresh it first via
///    [`update_branch`] (pull in place if checked out somewhere, otherwise
///    fetch + fast-forward the ref). When it has no upstream, that step is
///    skipped for feature targets (local-only merge of whatever `main` is);
///    for a target already on the default branch, returns a clear error.
/// 2. If `target` already has the default branch checked out, stop after the
///    refresh: [`MergeOutcome::FastForwarded`] when HEAD moved,
///    [`MergeOutcome::UpToDate`] when it was already current.
/// 3. Otherwise merge the (possibly refreshed) default into the target via
///    [`merge`]. When `autostash` is set, uncommitted local changes are stashed
///    before that merge and re-applied after (git's `--autostash`).
pub fn update(ctx: &Ctx, target: &str, autostash: bool) -> Result<MergeOutcome> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let default = git::default_branch(&ctx.repo_root)?;
    let on_default = info.branch.as_deref() == Some(default.as_str());
    let has_upstream = git::branch_upstream(&ctx.repo_root, &default)?.is_some();

    if has_upstream {
        if on_default {
            let dir = Path::new(&info.path);
            let before = git::short_hash(dir)?;
            update_branch(ctx, &default)?;
            let after = git::short_hash(dir)?;
            return if before == after {
                Ok(MergeOutcome::UpToDate)
            } else {
                Ok(MergeOutcome::FastForwarded { commit: after })
            };
        }
        update_branch(ctx, &default)?;
    } else if on_default {
        bail!(
            "worktree '{}' has the default branch '{default}' checked out, but \
             '{default}' has no upstream configured; push it first or set one \
             with `git branch --set-upstream-to`",
            info.name
        );
    }

    merge(ctx, target, &default, false, autostash)
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
    // Git's own labels are the first choice, but on a merge it writes the
    // useless "HEAD" for our side, and on a rebase it writes "HEAD" plus a bare
    // commit hash while the worktree sits on a detached HEAD. The side labels
    // are the whole basis for deciding which change to keep, so name them from
    // the rebase state files first, then the checked-out branch.
    let (rebase_ours, rebase_theirs) = if git::is_rebasing(dir) {
        git::rebase_side_names(dir)
    } else {
        (None, None)
    };
    let ours_label = marker_ours
        .filter(|l| l != "HEAD")
        .or(rebase_ours)
        .or_else(|| info.branch.clone())
        .unwrap_or_else(|| "HEAD".to_string());
    // Each in-progress operation names the incoming side with a different ref,
    // so try each in turn rather than assuming a merge.
    let theirs_label = rebase_theirs
        .or(marker_theirs)
        .or_else(|| {
            ["MERGE_HEAD", "REBASE_HEAD", "CHERRY_PICK_HEAD"]
                .iter()
                .find_map(|r| git::run(dir, &["rev-parse", "--short", r]).ok())
        })
        .unwrap_or_else(|| "incoming".to_string());
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

/// Writes `text` to `path` in the worktree named `target` **without** staging
/// it. Used to save a partly-resolved file: some hunks settled, the rest still
/// wrapped in conflict markers, so git rightly still sees the path as unmerged.
/// Reads `path` (relative to the worktree root) in the worktree named `target`
/// verbatim, conflict markers and all. This is what the resolver's whole-file
/// editor loads, so the user edits exactly the bytes git is looking at rather
/// than a re-rendering of the parsed hunks.
pub fn read_worktree_file(ctx: &Ctx, target: &str, path: &str) -> Result<String> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let full = Path::new(&info.path).join(path);
    std::fs::read_to_string(&full).with_context(|| format!("reading {}", full.display()))
}

pub fn write_partial_resolution(ctx: &Ctx, target: &str, path: &str, text: &str) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let full = Path::new(&info.path).join(path);
    std::fs::write(&full, text).with_context(|| format!("writing {}", full.display()))
}

/// Marks the conflicted `path` in the worktree named `target` resolved by
/// staging whatever is on disk right now, for work done outside wtm (in an
/// editor, or another terminal). Refuses while conflict markers remain, since
/// staging those would commit them.
pub fn stage_resolved(ctx: &Ctx, target: &str, path: &str) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    let full = dir.join(path);
    let text =
        std::fs::read_to_string(&full).with_context(|| format!("reading {}", full.display()))?;
    let segments = conflict::parse(&text);
    if conflict::has_conflicts(&segments) {
        let remaining = segments
            .iter()
            .filter(|s| matches!(s, conflict::ConflictSegment::Hunk { .. }))
            .count();
        bail!(
            "'{path}' still has {remaining} unresolved conflict hunk(s); \
             remove the <<<<<<< / ======= / >>>>>>> markers first"
        );
    }
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
    Ok(detect_resolve_kind_in(Path::new(&info.path)))
}

/// Marker-inspection half of [`detect_resolve_kind`], on an already-resolved
/// worktree directory.
fn detect_resolve_kind_in(dir: &Path) -> Option<ResolveKind> {
    match git::detect_in_progress(dir)? {
        git::InProgress::Merge => Some(ResolveKind::Merge),
        git::InProgress::Rebase => Some(ResolveKind::Rebase),
        git::InProgress::CherryPick => Some(ResolveKind::CherryPick),
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
                Some(msg) => git::commit(dir, msg, None)?,
                None => git::merge_continue(dir)?,
            }
            Some(git::short_hash(dir)?)
        }
        ResolveKind::Rebase => {
            if !git::is_rebasing(dir) {
                bail!("worktree '{}' has no rebase in progress", info.name);
            }
            git::rebase_continue(dir)?;
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
        ResolveKind::Rebase => git::rebase_abort(dir)?,
        ResolveKind::CherryPick => git::cherry_pick_abort(dir)?,
        ResolveKind::StashPop { .. } => git::reset_hard(dir)?,
    }
    Ok(())
}

/// Drops the commit a rebase is currently stopped on and carries on with the
/// rest, discarding that commit's changes. The escape hatch for a commit whose
/// changes are already present in the branch being rebased onto, where
/// `--continue` refuses because the result would be empty. Only meaningful for
/// a rebase; the other kinds have nothing to skip.
pub fn skip_resolution(ctx: &Ctx, target: &str, kind: ResolveKind) -> Result<()> {
    let info = find(ctx, target)?.ok_or_else(|| not_found(ctx, target))?;
    let dir = Path::new(&info.path);
    match kind {
        ResolveKind::Rebase => {
            if !git::is_rebasing(dir) {
                bail!("worktree '{}' has no rebase in progress", info.name);
            }
            git::rebase_skip(dir)?;
            Ok(())
        }
        other => bail!(
            "nothing to skip: worktree '{}' is resolving a {}, not a rebase",
            info.name,
            other.label()
        ),
    }
}

/// Switches the worktree named `name` to check out `branch`. Resolves `branch`
/// against local branches first, then falls back to the remotes, checking a
/// remote-only branch out as a new local branch that tracks it. When `create` is
/// set and no such branch exists anywhere, a new local branch of that name is
/// created off the worktree's current HEAD and checked out. Refuses when the
/// branch is already checked out in another worktree (git forbids this) or is
/// already the worktree's current branch.
pub fn switch_branch(ctx: &Ctx, name: &str, branch: &str, create: bool) -> Result<SwitchResult> {
    let info = find(ctx, name)?.ok_or_else(|| not_found(ctx, name))?;
    let dir = Path::new(&info.path);
    match resolve_switch_target(ctx, branch)? {
        Some((branch, remote_ref)) => {
            if info.branch.as_deref() == Some(branch.as_str()) {
                bail!(
                    "worktree '{}' already has '{branch}' checked out",
                    info.name
                );
            }
            if let Some(other) = list(ctx)?
                .into_iter()
                .find(|i| i.path != info.path && i.branch.as_deref() == Some(branch.as_str()))
            {
                bail!(
                    "branch '{branch}' is already checked out in worktree '{}'",
                    other.name
                );
            }
            match &remote_ref {
                Some(remote_ref) => git::switch_track(dir, &branch, remote_ref)?,
                None => git::switch(dir, &branch)?,
            }
            Ok(SwitchResult {
                name: branch.clone(),
                branch,
                path: info.path,
            })
        }
        // No branch of that name exists locally or on any remote.
        None if create => {
            let branch = branch.trim();
            if branch.is_empty() {
                bail!("branch name must not be empty");
            }
            git::switch_create(dir, branch)?;
            Ok(SwitchResult {
                name: branch.to_string(),
                branch: branch.to_string(),
                path: info.path,
            })
        }
        None => bail!("no branch named '{branch}' (searched local branches and remotes)"),
    }
}

/// Resolves what a caller asked to switch to into the local branch name to check
/// out and, when that branch does not exist locally yet, the remote ref to create
/// it from. Accepts a local branch name, a remote-only branch's short name, or a
/// fully qualified `<remote>/<branch>` ref. Returns `Ok(None)` when nothing of
/// that name exists locally or on a remote (so a caller may choose to create it);
/// an ambiguous remote match is still a hard error.
fn resolve_switch_target(ctx: &Ctx, branch: &str) -> Result<Option<(String, Option<String>)>> {
    if git::branch_exists(&ctx.repo_root, branch) {
        return Ok(Some((branch.to_string(), None)));
    }
    let remotes = git::remote_branches(&ctx.repo_root)?;
    // A fully qualified ref is unambiguous, so honor that spelling first; the
    // local branch it creates is named after the branch, not the remote.
    let resolved = match remotes.iter().find(|(_, full)| full == branch) {
        Some(hit) => hit.clone(),
        None => {
            let mut matches = remotes.iter().filter(|(short, _)| short == branch);
            let Some(first) = matches.next() else {
                return Ok(None);
            };
            if matches.next().is_some() {
                bail!(
                    "branch '{branch}' exists on more than one remote; use a '<remote>/{branch}' name"
                );
            }
            first.clone()
        }
    };
    let (short, remote_ref) = resolved;
    // `<remote>/<branch>` can resolve onto a branch that does exist locally, in
    // which case switch to the local branch instead of trying to recreate it.
    if git::branch_exists(&ctx.repo_root, &short) {
        return Ok(Some((short, None)));
    }
    Ok(Some((short, Some(remote_ref))))
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

/// Whether `branch` has been merged into `default`, given `default`'s
/// first-parent trunk (from [`git::first_parent_commits`], computed once per
/// listing). A branch counts as merged only when it is fully contained in
/// `default` (no commits of its own left outstanding) AND its tip sits *off*
/// `default`'s first-parent trunk, i.e. it was merged in from the side via a
/// merge commit. A brand-new branch, or one that is merely behind `default`
/// without ever diverging, has its tip on the trunk and so is not reported as
/// merged (it has nothing that was actually merged in). Fast-forward merges,
/// which leave no merge commit, are the known blind spot: the branch becomes
/// part of the trunk and is indistinguishable from an ordinary ancestor.
fn branch_merged_into(
    dir: &Path,
    default: &str,
    branch: &str,
    trunk: &HashSet<String>,
) -> Result<bool> {
    if branch == default {
        return Ok(false);
    }
    if git::commits_ahead_of(dir, default, branch)? != 0 {
        return Ok(false);
    }
    let tip = git::rev_parse(dir, branch)?;
    Ok(!trunk.contains(&tip))
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

    /// Simulates a teammate's fetched branch: `<remote>/<branch>` pointing at
    /// HEAD, with no local branch of its own. The remote is registered (but
    /// never fetched from), since git only treats the ref as a remote-tracking
    /// branch when its remote is configured.
    fn make_remote_ref(ctx: &Ctx, remote: &str, branch: &str) {
        if !git::remotes(&ctx.repo_root)
            .unwrap()
            .iter()
            .any(|r| r == remote)
        {
            git(
                &ctx.repo_root,
                &["remote", "add", remote, "https://example.invalid/repo.git"],
            );
        }
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&ctx.repo_root)
            .output()
            .unwrap();
        let sha = String::from_utf8(out.stdout).unwrap();
        git(
            &ctx.repo_root,
            &[
                "update-ref",
                &format!("refs/remotes/{remote}/{branch}"),
                sha.trim(),
            ],
        );
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
    fn switch_checks_out_remote_only_branch_as_tracking_branch() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feat");
        make_remote_ref(&ctx, "origin", "teammate");
        assert!(!git::branch_exists(&ctx.repo_root, "teammate"));

        let r = switch_branch(&ctx, "feat", "teammate", false).unwrap();
        assert_eq!(r.branch, "teammate");
        // The remote-only branch now exists locally, tracking the remote.
        assert!(git::branch_exists(&ctx.repo_root, "teammate"));
        assert_eq!(
            git::branch_upstream(&ctx.repo_root, "teammate").unwrap(),
            Some(("origin".to_string(), "teammate".to_string()))
        );
    }

    #[test]
    fn switch_accepts_fully_qualified_remote_ref() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feat");
        make_remote_ref(&ctx, "origin", "teammate");

        // `origin/teammate` names the same branch; the local one it creates is
        // named for the branch, not the remote.
        let r = switch_branch(&ctx, "feat", "origin/teammate", false).unwrap();
        assert_eq!(r.branch, "teammate");
        assert!(!git::branch_exists(&ctx.repo_root, "origin/teammate"));
    }

    #[test]
    fn switch_to_unknown_branch_errors() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feat");
        let err = switch_branch(&ctx, "feat", "ghost", false).unwrap_err();
        assert!(
            err.to_string().contains("no branch named 'ghost'"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn switch_with_create_makes_a_new_branch_off_head() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feat");
        assert!(!git::branch_exists(&ctx.repo_root, "brand-new"));

        let r = switch_branch(&ctx, "feat", "brand-new", true).unwrap();
        assert_eq!(r.branch, "brand-new");
        // The new branch exists and is what the worktree now has checked out.
        assert!(git::branch_exists(&ctx.repo_root, "brand-new"));
        let info = find(&ctx, "brand-new").unwrap().unwrap();
        assert_eq!(info.branch.as_deref(), Some("brand-new"));
        assert_eq!(info.path, path.to_string_lossy());
    }

    #[test]
    fn switch_with_create_still_switches_onto_an_existing_branch() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feat");
        git::branch_create(&ctx.repo_root, "existing", None).unwrap();

        // `create` only kicks in when the branch is missing; an existing branch
        // is switched onto, not recreated (which would error).
        let r = switch_branch(&ctx, "feat", "existing", true).unwrap();
        assert_eq!(r.branch, "existing");
    }

    #[test]
    fn switch_to_branch_on_multiple_remotes_asks_to_disambiguate() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feat");
        make_remote_ref(&ctx, "origin", "shared");
        make_remote_ref(&ctx, "upstream", "shared");

        let err = switch_branch(&ctx, "feat", "shared", false).unwrap_err();
        assert!(
            err.to_string().contains("more than one remote"),
            "unexpected error: {err:#}"
        );
        // The fully qualified spelling resolves it.
        let r = switch_branch(&ctx, "feat", "upstream/shared", false).unwrap();
        assert_eq!(r.branch, "shared");
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

        let outcome = merge(&ctx, "feature", "main", false, false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        assert!(path.join("main.txt").exists());
        assert!(!git::is_merging(&path));

        // A second merge has nothing new to bring in.
        let outcome = merge(&ctx, "feature", "main", false, false).unwrap();
        assert!(matches!(outcome, MergeOutcome::UpToDate), "{outcome:?}");
    }

    /// Sets up `feature` and `main` with edits to the same line, so any attempt
    /// to combine them conflicts. Returns the feature worktree's path.
    fn diverged_worktree(ctx: &Ctx) -> std::path::PathBuf {
        std::fs::write(ctx.repo_root.join("shared.txt"), "base\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(ctx, "feature");
        std::fs::write(ctx.repo_root.join("shared.txt"), "main version\n").unwrap();
        git(&ctx.repo_root, &["commit", "-am", "main edit"]);
        std::fs::write(path.join("shared.txt"), "feature version\n").unwrap();
        git(&path, &["commit", "-am", "feature edit"]);
        path
    }

    #[test]
    fn rebase_replays_commits_onto_the_target() {
        let (_tmp, ctx) = temp_ctx();
        // Non-overlapping edits, so the replay is clean.
        std::fs::write(ctx.repo_root.join("a.txt"), "a\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "base"]);
        let path = make_worktree(&ctx, "feature");
        std::fs::write(ctx.repo_root.join("main-only.txt"), "m\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "main work"]);
        std::fs::write(path.join("feat-only.txt"), "f\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "feature work"]);

        let outcome = rebase(&ctx, "feature", "main", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        assert!(!git::is_rebasing(&path));
        // The replayed branch now contains main's commit as an ancestor, which
        // is the whole point of rebasing.
        assert!(
            path.join("main-only.txt").exists(),
            "main work is underneath"
        );
        assert!(
            path.join("feat-only.txt").exists(),
            "feature work is on top"
        );
    }

    #[test]
    fn rebase_conflict_leaves_tree_mid_rebase_and_abort_recovers() {
        let (_tmp, ctx) = temp_ctx();
        let path = diverged_worktree(&ctx);

        let outcome = rebase(&ctx, "feature", "main", false).unwrap();
        let MergeOutcome::Conflicted { files } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        assert_eq!(files, vec!["shared.txt".to_string()]);
        // Left mid-rebase so the resolver can take over, and detected as a
        // rebase rather than as the cherry-pick its machinery uses.
        assert!(git::is_rebasing(&path));
        assert_eq!(
            detect_resolve_kind(&ctx, "feature").unwrap(),
            Some(ResolveKind::Rebase)
        );

        abort_resolution(&ctx, "feature", ResolveKind::Rebase).unwrap();
        assert!(!git::is_rebasing(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "feature version\n"
        );
    }

    #[test]
    fn rebase_conflict_resolves_and_continues() {
        let (_tmp, ctx) = temp_ctx();
        let path = diverged_worktree(&ctx);
        assert!(matches!(
            rebase(&ctx, "feature", "main", false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));

        write_resolution(&ctx, "feature", "shared.txt", "resolved\n").unwrap();
        let done = complete_resolution(&ctx, "feature", ResolveKind::Rebase, None).unwrap();
        assert!(done.commit.is_some());
        assert!(!git::is_rebasing(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[test]
    fn rebase_skip_drops_the_stopped_commit() {
        let (_tmp, ctx) = temp_ctx();
        let path = diverged_worktree(&ctx);
        assert!(matches!(
            rebase(&ctx, "feature", "main", false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));

        skip_resolution(&ctx, "feature", ResolveKind::Rebase).unwrap();
        assert!(
            !git::is_rebasing(&path),
            "only one commit, so skipping ends it"
        );
        // The feature commit was dropped, leaving main's version in place.
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "main version\n"
        );
    }

    #[test]
    fn skip_resolution_refuses_for_a_merge() {
        let (_tmp, ctx) = temp_ctx();
        diverged_worktree(&ctx);
        assert!(matches!(
            merge(&ctx, "feature", "main", false, false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));
        let err = skip_resolution(&ctx, "feature", ResolveKind::Merge)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a rebase"), "{err}");
    }

    #[test]
    fn rebase_refuses_when_another_operation_is_in_progress() {
        let (_tmp, ctx) = temp_ctx();
        diverged_worktree(&ctx);
        assert!(matches!(
            merge(&ctx, "feature", "main", false, false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));
        let err = rebase(&ctx, "feature", "main", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge in progress"), "{err}");
    }

    #[test]
    fn rebase_rejects_an_unknown_branch() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feature");
        let err = rebase(&ctx, "feature", "nope", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no local or remote branch"), "{err}");
    }

    #[test]
    fn stage_resolved_refuses_while_markers_remain() {
        let (_tmp, ctx) = temp_ctx();
        let path = diverged_worktree(&ctx);
        assert!(matches!(
            merge(&ctx, "feature", "main", false, false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));

        // Straight from git, the file is still full of markers.
        let err = stage_resolved(&ctx, "feature", "shared.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unresolved conflict"), "{err}");
        assert_eq!(git::conflicted_files(&path).unwrap().len(), 1);

        // Resolved by hand outside wtm, it stages as-is.
        std::fs::write(path.join("shared.txt"), "by hand\n").unwrap();
        stage_resolved(&ctx, "feature", "shared.txt").unwrap();
        assert!(git::conflicted_files(&path).unwrap().is_empty());
    }

    #[test]
    fn write_partial_resolution_saves_without_staging() {
        let (_tmp, ctx) = temp_ctx();
        let path = diverged_worktree(&ctx);
        assert!(matches!(
            merge(&ctx, "feature", "main", false, false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));

        write_partial_resolution(&ctx, "feature", "shared.txt", "half done\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("shared.txt")).unwrap(),
            "half done\n",
            "written to disk"
        );
        assert_eq!(
            git::conflicted_files(&path).unwrap(),
            vec!["shared.txt".to_string()],
            "but still unmerged, since nothing was staged"
        );
    }

    #[test]
    fn list_reports_conflicts_and_the_operation_in_progress() {
        let (_tmp, ctx) = temp_ctx();
        diverged_worktree(&ctx);
        let before = list(&ctx).unwrap();
        let feature = before.iter().find(|w| w.name == "feature").unwrap();
        assert_eq!(feature.conflicted, 0);
        assert_eq!(feature.in_progress, None);

        assert!(matches!(
            rebase(&ctx, "feature", "main", false).unwrap(),
            MergeOutcome::Conflicted { .. }
        ));
        let after = list(&ctx).unwrap();
        let feature = after.iter().find(|w| w.name == "feature").unwrap();
        assert_eq!(feature.conflicted, 1, "the list flags the unmerged file");
        assert_eq!(feature.in_progress, Some(ResolveKind::Rebase));
        // The main worktree is untouched by the feature worktree's rebase.
        let main = after.iter().find(|w| w.is_main).unwrap();
        assert_eq!(main.in_progress, None);
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

        let outcome = merge(&ctx, "feature", "main", false, false).unwrap();
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

        let outcome = merge(&ctx, "feature", "main", false, false).unwrap();
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
        assert!(merge(&ctx, "feature", "nope", false, false).is_err());
        assert!(merge(&ctx, "feature", "feature", false, false).is_err());
    }

    #[test]
    fn update_merges_default_branch_locally_without_upstream() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(ctx.repo_root.join("new.txt"), "x\n").unwrap();
        git(&ctx.repo_root, &["add", "."]);
        git(&ctx.repo_root, &["commit", "-m", "advance main"]);

        // No remote: still merges the local default into the feature worktree.
        let outcome = update(&ctx, "feature", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        assert!(path.join("new.txt").exists());

        // On the default branch with no upstream: clear error, not a silent no-op.
        let err = update(&ctx, "main", false).unwrap_err().to_string();
        assert!(
            err.contains("no upstream"),
            "expected no-upstream error, got: {err}"
        );
    }

    /// When the target is on the default branch and that branch tracks a remote,
    /// update refreshes it in place (fetch + fast-forward) instead of refusing.
    #[test]
    fn update_on_default_fast_forwards_from_upstream() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        advance_remote(tmp.path(), &bare, "main", "remote-work");

        let before = git::run(&ctx.repo_root, &["rev-parse", "main"]).unwrap();
        let outcome = update(&ctx, "main", false).unwrap();
        let MergeOutcome::FastForwarded { commit } = outcome else {
            panic!("expected FastForwarded, got {outcome:?}");
        };
        let after = git::run(&ctx.repo_root, &["rev-parse", "main"]).unwrap();
        assert_ne!(before, after, "main should have moved");
        assert_eq!(commit, git::short_hash(&ctx.repo_root).unwrap());
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s", "main"]).unwrap(),
            "remote-work"
        );

        // A second update finds nothing new.
        let again = update(&ctx, "main", false).unwrap();
        assert!(matches!(again, MergeOutcome::UpToDate), "{again:?}");
    }

    /// Updating a feature worktree refreshes the default from its upstream
    /// first, then merges that tip into the feature branch.
    #[test]
    fn update_on_feature_refreshes_default_then_merges() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        let path = make_worktree(&ctx, "feature");
        advance_remote(tmp.path(), &bare, "main", "remote-work");

        let main_before = git::run(&ctx.repo_root, &["rev-parse", "main"]).unwrap();
        let outcome = update(&ctx, "feature", false).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }), "{outcome:?}");
        let main_after = git::run(&ctx.repo_root, &["rev-parse", "main"]).unwrap();
        assert_ne!(main_before, main_after, "main should have been refreshed");
        assert_eq!(
            git::run(&path, &["log", "-1", "--format=%s", "main"]).unwrap(),
            "remote-work"
        );
        // Feature's history now includes the remote commit (via merge of main).
        let feature_log = git::run(&path, &["log", "--oneline"]).unwrap();
        assert!(
            feature_log.contains("remote-work"),
            "feature should contain remote-work after update: {feature_log}"
        );
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

    /// Item 8: a worktree whose branch's commits have all landed in the default
    /// branch is flagged merged; a brand-new branch that merely points at main's
    /// tip is not (nothing has been merged), nor is one with its own outstanding
    /// commit, nor the main worktree.
    #[test]
    fn list_flags_merged_worktrees() {
        let (_tmp, ctx) = temp_ctx();
        // A branch created off the original main that never gets any work: it
        // ends up *behind* main once main advances, but was never merged.
        make_worktree(&ctx, "stale-branch");
        // A branch whose work has actually been merged back into main.
        let merged_wt = make_worktree(&ctx, "merged-branch");
        std::fs::write(merged_wt.join("f.txt"), "x\n").unwrap();
        git(&merged_wt, &["add", "f.txt"]);
        git(&merged_wt, &["commit", "-m", "merged work"]);
        // This merge advances main past stale-branch's tip.
        git(
            &ctx.repo_root,
            &["merge", "--no-ff", "-m", "merge", "merged-branch"],
        );
        // A brand-new branch off the advanced main, with no work of its own.
        make_worktree(&ctx, "fresh-branch");
        // A branch with its own commit not in main: not merged.
        let ahead = make_worktree(&ctx, "ahead-branch");
        std::fs::write(ahead.join("f.txt"), "y\n").unwrap();
        git(&ahead, &["add", "f.txt"]);
        git(&ahead, &["commit", "-m", "ahead work"]);

        let infos = list(&ctx).unwrap();
        let merged = |name: &str| infos.iter().find(|i| i.name == name).unwrap().merged;
        assert!(
            merged("merged-branch"),
            "branch merged into main is flagged"
        );
        assert!(!merged("fresh-branch"), "brand-new branch is not merged");
        assert!(
            !merged("stale-branch"),
            "a branch merely behind main was never merged"
        );
        assert!(!merged("ahead-branch"), "branch with own commit is not");
        // The main worktree is never flagged.
        assert!(!infos.iter().find(|i| i.is_main).unwrap().merged);
    }

    /// `create` records the base in `.wtm.toml` `[created_from]`; a fresh
    /// worktree is unchanged, a commit on it flips `changed_from_base`, and a
    /// commit on the base flips `behind_base`. The main worktree stays quiet.
    #[test]
    fn list_flags_changed_and_outdated_vs_creation_base() {
        let (_tmp, ctx) = temp_ctx();
        create(&ctx, "feature", Some("main"), RunMode::Capture, |_| {}).unwrap();

        let map = crate::config::load_created_from(&ctx.repo_root).unwrap();
        assert_eq!(map.get("feature"), Some(&"main".to_string()));

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert_eq!(feature.created_from.as_deref(), Some("main"));
        assert!(
            !feature.changed_from_base,
            "fresh branch matching main is unchanged"
        );
        assert!(!feature.behind_base, "main has not moved ahead yet");
        let main = infos.iter().find(|i| i.is_main).unwrap();
        assert!(main.created_from.is_none());
        assert!(!main.changed_from_base);
        assert!(!main.behind_base);

        let feature_path = PathBuf::from(&feature.path);
        std::fs::write(feature_path.join("f.txt"), "x\n").unwrap();
        git(&feature_path, &["add", "f.txt"]);
        git(&feature_path, &["commit", "-m", "feature work"]);

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert!(feature.changed_from_base, "unique commits vs creation base");
        assert!(!feature.behind_base);

        git(
            &ctx.repo_root,
            &["commit", "--allow-empty", "-m", "main moved"],
        );
        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert!(feature.changed_from_base, "still has unique commits");
        assert!(
            feature.behind_base,
            "creation base main is ahead of feature"
        );
    }

    /// A deleted creation base falls through to the default branch tip.
    #[test]
    fn list_falls_back_to_default_when_creation_base_is_gone() {
        let (_tmp, ctx) = temp_ctx();
        create(&ctx, "feature", Some("main"), RunMode::Capture, |_| {}).unwrap();
        crate::config::set_created_from(&ctx.repo_root, "feature", "vanished").unwrap();
        git(&ctx.repo_root, &["branch", "vanished"]);
        git(&ctx.repo_root, &["branch", "-D", "vanished"]);

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert_eq!(
            feature.created_from.as_deref(),
            Some("main"),
            "missing recorded base falls back to default"
        );
        assert!(!feature.changed_from_base);
        assert!(!feature.behind_base);

        git(
            &ctx.repo_root,
            &["commit", "--allow-empty", "-m", "main moved"],
        );
        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert!(
            feature.behind_base,
            "default-branch fallback still detects outdated"
        );
    }

    /// Without a `[created_from]` entry, list still compares against the default
    /// branch so older / existing-branch worktrees get same/changed/outdated.
    #[test]
    fn list_flags_vs_default_branch_without_created_from() {
        let (_tmp, ctx) = temp_ctx();
        create(&ctx, "feature", Some("main"), RunMode::Capture, |_| {}).unwrap();
        crate::config::unset_created_from(&ctx.repo_root, "feature").unwrap();

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert_eq!(feature.created_from.as_deref(), Some("main"));
        assert!(!feature.changed_from_base);
        assert!(!feature.behind_base);
        assert!(
            feature.flag_labels().contains(&"same"),
            "fresh feature vs default shows same: {:?}",
            feature.flag_labels()
        );

        let feature_path = PathBuf::from(&feature.path);
        std::fs::write(feature_path.join("f.txt"), "x\n").unwrap();
        git(&feature_path, &["add", "f.txt"]);
        git(&feature_path, &["commit", "-m", "feature work"]);
        git(
            &ctx.repo_root,
            &["commit", "--allow-empty", "-m", "main moved"],
        );

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert!(feature.changed_from_base);
        assert!(feature.behind_base);
        let flags = feature.flag_labels();
        assert!(flags.contains(&"changed"), "{flags:?}");
        assert!(flags.contains(&"outdated"), "{flags:?}");
    }

    /// Merge-base comparison reports unique commits but never `outdated`.
    #[test]
    fn base_status_merge_base_kind_is_changed_not_outdated() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        git(&path, &["add", "f.txt"]);
        git(&path, &["commit", "-m", "feature work"]);

        let mb = git::merge_base(&ctx.repo_root, "feature", "main").unwrap();
        let status = base_status_vs(&ctx.repo_root, &mb, "feature", BaseCompareKind::MergeBase);
        assert!(status.changed, "feature is ahead of the fork point");
        assert!(!status.behind, "merge-base kind cannot be outdated");
    }

    /// When the named default tip is missing, resolve falls back to merge-base
    /// against origin/HEAD and still flags unique commits (not outdated).
    #[test]
    fn resolve_base_status_merge_base_via_origin_head() {
        let (tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        git(&path, &["add", "f.txt"]);
        git(&path, &["commit", "-m", "feature work"]);

        let bare = tmp.path().join("bare.git");
        git(
            tmp.path(),
            &[
                "clone",
                "--bare",
                ctx.repo_root.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(
            &ctx.repo_root,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&ctx.repo_root, &["fetch", "origin"]);
        git(
            &ctx.repo_root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        // Bogus default name: tip paths miss; origin/HEAD still resolves.
        let status = resolve_base_status(&ctx.repo_root, "feature", None, Some("nope"));
        assert_eq!(status.label.as_deref(), Some("nope"));
        assert!(status.changed);
        assert!(!status.behind);
    }

    /// Upstream ahead/behind become `unpushed` / `behind` in flag labels.
    #[test]
    fn list_flag_labels_unpushed_and_behind_upstream() {
        let (tmp, ctx) = temp_ctx();
        let bare = tmp.path().join("bare.git");
        git(
            tmp.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(
            &ctx.repo_root,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&ctx.repo_root, &["push", "-u", "origin", "main"]);

        let path = make_worktree(&ctx, "feature");
        git(&path, &["push", "-u", "origin", "feature"]);

        // Local commit → unpushed.
        std::fs::write(path.join("local.txt"), "mine\n").unwrap();
        git(&path, &["add", "local.txt"]);
        git(&path, &["commit", "-m", "local"]);
        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        assert!(
            feature.flag_labels().contains(&"unpushed"),
            "{:?}",
            feature.flag_labels()
        );

        // Advance remote without pulling → behind (and still unpushed).
        let second = tmp.path().join("second");
        git(
            tmp.path(),
            &["clone", bare.to_str().unwrap(), second.to_str().unwrap()],
        );
        git(&second, &["config", "user.email", "t@e.st"]);
        git(&second, &["config", "user.name", "t"]);
        git(&second, &["checkout", "-B", "feature", "origin/feature"]);
        std::fs::write(second.join("remote.txt"), "theirs\n").unwrap();
        git(&second, &["add", "remote.txt"]);
        git(&second, &["commit", "-m", "remote"]);
        git(&second, &["push"]);
        git(&ctx.repo_root, &["fetch", "origin"]);

        let infos = list(&ctx).unwrap();
        let feature = infos.iter().find(|i| i.name == "feature").unwrap();
        let flags = feature.flag_labels();
        assert!(flags.contains(&"unpushed"), "{flags:?}");
        assert!(flags.contains(&"behind"), "{flags:?}");
    }

    /// Branch list carries the same base flags as worktree list.
    #[test]
    fn branch_list_flags_changed_and_outdated_vs_default() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        crate::config::unset_created_from(&ctx.repo_root, "feature").unwrap();
        std::fs::write(path.join("f.txt"), "x\n").unwrap();
        git(&path, &["add", "f.txt"]);
        git(&path, &["commit", "-m", "feature work"]);
        git(
            &ctx.repo_root,
            &["commit", "--allow-empty", "-m", "main moved"],
        );

        let branches = branch_list(&ctx).unwrap().branches;
        let feature = branches.iter().find(|b| b.name == "feature").unwrap();
        assert_eq!(feature.created_from.as_deref(), Some("main"));
        assert!(feature.changed_from_base);
        assert!(feature.behind_base);
        let flags = feature.flag_labels();
        assert!(flags.contains(&"changed"), "{flags:?}");
        assert!(flags.contains(&"outdated"), "{flags:?}");

        let main = branches.iter().find(|b| b.name == "main").unwrap();
        assert!(main.created_from.is_none());
        assert!(!main.changed_from_base);
        assert!(!main.behind_base);
    }

    /// Removing a worktree clears its `[created_from]` entry; renaming moves it.
    #[test]
    fn created_from_follows_rename_and_clears_on_remove() {
        let (_tmp, ctx) = temp_ctx();
        create(&ctx, "feature", Some("main"), RunMode::Capture, |_| {}).unwrap();
        rename_worktree(&ctx, "feature", "renamed").unwrap();
        let map = crate::config::load_created_from(&ctx.repo_root).unwrap();
        assert!(!map.contains_key("feature"));
        assert_eq!(map.get("renamed"), Some(&"main".to_string()));

        remove_worktree_only(&ctx, "renamed", false).unwrap();
        assert!(
            crate::config::load_created_from(&ctx.repo_root)
                .unwrap()
                .is_empty()
        );
    }

    /// Default create (no `--from`) persists the main worktree's branch, not
    /// the literal `HEAD`, so later base-ahead checks track that branch.
    #[test]
    fn create_without_from_persists_head_branch_name() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "from-head");
        let map = crate::config::load_created_from(&ctx.repo_root).unwrap();
        assert_eq!(map.get("from-head"), Some(&"main".to_string()));
    }

    /// The Branches tab's merged flag mirrors the worktree merged flag: a branch
    /// whose work has landed in main is flagged, a brand-new one is not, and the
    /// default branch itself is never flagged.
    #[test]
    fn branch_list_flags_merged_branches() {
        let (_tmp, ctx) = temp_ctx();
        let merged_wt = make_worktree(&ctx, "merged-branch");
        std::fs::write(merged_wt.join("f.txt"), "x\n").unwrap();
        git(&merged_wt, &["add", "f.txt"]);
        git(&merged_wt, &["commit", "-m", "merged work"]);
        git(
            &ctx.repo_root,
            &["merge", "--no-ff", "-m", "merge", "merged-branch"],
        );
        make_worktree(&ctx, "fresh-branch");

        let result = branch_list(&ctx).unwrap();
        let merged = |name: &str| {
            result
                .branches
                .iter()
                .find(|b| b.name == name)
                .unwrap()
                .merged
        };
        assert!(merged("merged-branch"), "merged branch is flagged");
        assert!(!merged("fresh-branch"), "brand-new branch is not");
        assert!(!merged("main"), "the default branch is never flagged");
    }

    /// A branch that exists only on the remote (never fetched into a local
    /// branch) shows up as its own row, tagged with the remote it lives on,
    /// rather than being absent from the list entirely.
    #[test]
    fn branch_list_surfaces_remote_only_branches() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        // Push a branch to the remote from an independent clone, so it lands
        // on "origin" without ever existing as a local branch in `ctx`.
        let clone = tmp.path().join("clone-remote-only");
        git(
            tmp.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.email", "t@e.st"]);
        git(&clone, &["config", "user.name", "t"]);
        git(&clone, &["checkout", "-b", "teammate-feature"]);
        git(&clone, &["commit", "--allow-empty", "-m", "remote work"]);
        git(&clone, &["push", "origin", "teammate-feature"]);
        git(&ctx.repo_root, &["fetch", "origin"]);

        let result = branch_list(&ctx).unwrap();
        let item = result
            .branches
            .iter()
            .find(|b| b.name == "teammate-feature")
            .expect("remote-only branch is listed");
        assert_eq!(item.remote.as_deref(), Some("origin/teammate-feature"));
        assert_eq!(item.checked_out_path, None, "not checked out anywhere yet");

        // A local branch with the same name as an existing local branch is
        // never duplicated as a remote-only row.
        assert_eq!(
            result.branches.iter().filter(|b| b.name == "main").count(),
            1,
            "main exists locally and on origin, but appears once"
        );
    }

    /// Renaming a worktree renames its branch and moves its directory to a
    /// sibling folder named after the new name, keeping it addressable.
    #[test]
    fn rename_worktree_renames_branch_and_moves_directory() {
        let (_tmp, ctx) = temp_ctx();
        let old_path = make_worktree(&ctx, "feature");

        let result = rename_worktree(&ctx, "feature", "renamed").unwrap();
        assert!(result.renamed_branch);
        assert_eq!(result.new_name, "renamed");

        // The old branch and directory are gone; the new ones are in place.
        assert!(!git::branch_exists(&ctx.repo_root, "feature"));
        assert!(git::branch_exists(&ctx.repo_root, "renamed"));
        assert!(!old_path.exists(), "old directory moved");
        assert!(
            PathBuf::from(&result.new_path).exists(),
            "new directory present"
        );

        // It is addressable by the new name and reports the renamed branch.
        let info = find(&ctx, "renamed").unwrap().unwrap();
        assert_eq!(info.branch.as_deref(), Some("renamed"));
    }

    /// The main worktree is the repository itself and cannot be renamed.
    #[test]
    fn rename_worktree_refuses_the_main_worktree() {
        let (_tmp, ctx) = temp_ctx();
        let main = list(&ctx)
            .unwrap()
            .into_iter()
            .find(|i| i.is_main)
            .unwrap()
            .name;
        let err = rename_worktree(&ctx, &main, "whatever").unwrap_err();
        assert!(err.to_string().contains("main worktree"), "{err}");
    }

    /// Item 4: a commit's changed files and a single file's diff are readable.
    #[test]
    fn commit_files_and_diff_report_a_commits_changes() {
        let (_tmp, ctx) = temp_ctx();
        std::fs::write(ctx.repo_root.join("a.txt"), "hello\n").unwrap();
        git(&ctx.repo_root, &["add", "a.txt"]);
        git(&ctx.repo_root, &["commit", "-m", "add a"]);
        let hash = git::run(&ctx.repo_root, &["rev-parse", "HEAD"]).unwrap();

        // The main worktree's name is how ops addresses it.
        let main = list(&ctx).unwrap();
        let name = main.iter().find(|i| i.is_main).unwrap().name.clone();

        let files = commit_files(&ctx, &name, &hash).unwrap();
        assert!(files.iter().any(|f| f.path == "a.txt"), "{files:?}");
        let diff = commit_file_diff(&ctx, &name, &hash, "a.txt").unwrap();
        assert!(diff.contains("hello"), "diff shows the added line: {diff}");
    }

    /// Item 6: updating a dirty worktree with autostash stashes the local edit,
    /// merges the mainline, and reapplies the edit afterwards.
    #[test]
    fn update_with_autostash_preserves_local_changes() {
        let (_tmp, ctx) = temp_ctx();
        // Give the worktree its own history so an update actually merges.
        let wt = make_worktree(&ctx, "feature");
        // Advance main so there is something to pull in.
        std::fs::write(ctx.repo_root.join("main.txt"), "main\n").unwrap();
        git(&ctx.repo_root, &["add", "main.txt"]);
        git(&ctx.repo_root, &["commit", "-m", "main work"]);
        // Leave an uncommitted local change in the worktree.
        std::fs::write(wt.join("local.txt"), "work in progress\n").unwrap();

        let outcome = update(&ctx, "feature", true).unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }));
        // The mainline commit landed and the local change was reapplied.
        assert!(wt.join("main.txt").exists(), "mainline change merged in");
        assert_eq!(
            std::fs::read_to_string(wt.join("local.txt")).unwrap(),
            "work in progress\n",
            "local edit reapplied after the update"
        );
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
        let outcome = merge(ctx, "feature", "main", false, false).unwrap();
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
        // Git's markers label ours "HEAD", which names nothing; the label
        // reported is the branch actually checked out there.
        assert_eq!(file.ours_label, "feature");
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
    fn move_changes_moves_uncommitted_work_between_worktrees() {
        let (_tmp, ctx) = temp_ctx();
        let from_path = make_worktree(&ctx, "feature");
        let to_path = make_worktree(&ctx, "other");
        std::fs::write(from_path.join("a.txt"), "a\n").unwrap();
        std::fs::write(from_path.join("b.txt"), "b\n").unwrap();

        let result = move_changes(&ctx, "feature", "other").unwrap();
        assert_eq!(result.from, "feature");
        assert_eq!(result.to, "other");
        assert_eq!(result.files, 2);

        assert!(git::status(&from_path).unwrap().is_empty());
        assert_eq!(git::status(&to_path).unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(to_path.join("a.txt")).unwrap(),
            "a\n"
        );
        // No stash left behind on either side.
        assert!(git::stash_list(&from_path).unwrap().is_empty());
    }

    #[test]
    fn move_changes_errors_when_source_is_clean() {
        let (_tmp, ctx) = temp_ctx();
        make_worktree(&ctx, "feature");
        make_worktree(&ctx, "other");

        let err = move_changes(&ctx, "feature", "other").unwrap_err();
        assert!(
            err.to_string().contains("no uncommitted changes"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn move_changes_errors_when_destination_is_dirty() {
        let (_tmp, ctx) = temp_ctx();
        let from_path = make_worktree(&ctx, "feature");
        let to_path = make_worktree(&ctx, "other");
        std::fs::write(from_path.join("a.txt"), "a\n").unwrap();
        std::fs::write(to_path.join("b.txt"), "b\n").unwrap();

        let err = move_changes(&ctx, "feature", "other").unwrap_err();
        assert!(
            err.to_string().contains("uncommitted changes of its own"),
            "unexpected error: {err:#}"
        );
        // Nothing was stashed; the source is untouched.
        assert_eq!(git::status(&from_path).unwrap().len(), 1);
    }

    #[test]
    fn move_changes_rejects_moving_into_itself() {
        let (_tmp, ctx) = temp_ctx();
        let path = make_worktree(&ctx, "feature");
        std::fs::write(path.join("a.txt"), "a\n").unwrap();

        let err = move_changes(&ctx, "feature", "feature").unwrap_err();
        assert!(
            err.to_string().contains("same worktree"),
            "unexpected error: {err:#}"
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

    /// A pull of a diverged worktree turns the refused fast-forward into a
    /// hint pointing at `--rebase`, and the error is recognizably
    /// non-fast-forward so the TUI can offer the retry.
    #[test]
    fn pull_of_a_diverged_worktree_hints_at_rebase() {
        let (tmp, ctx) = temp_ctx();
        let bare = with_origin(tmp.path(), &ctx);
        advance_remote(tmp.path(), &bare, "main", "remote-work");
        // A different local commit forks the histories.
        git(&ctx.repo_root, &["commit", "--allow-empty", "-m", "local"]);

        let err = pull(&ctx, "main", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--rebase"), "unexpected error: {msg}");
        assert!(git::is_non_fast_forward(&msg), "unexpected error: {msg}");

        // The rebase retry the hint suggests completes and keeps both sides.
        let r = pull(&ctx, "main", true).unwrap();
        assert!(!r.already_up_to_date);
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s"]).unwrap(),
            "local"
        );
        assert_eq!(
            git::run(&ctx.repo_root, &["log", "-1", "--format=%s", "HEAD~1"]).unwrap(),
            "remote-work"
        );
    }
}
