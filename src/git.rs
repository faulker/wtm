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

/// Runs git and reports only whether it exited zero. Used for predicate
/// commands like `diff --quiet` where a clean non-zero exit means "differs",
/// not "failed". Only a failure to spawn git is surfaced as an error.
pub fn run_predicate(dir: &Path, args: &[&str]) -> Result<bool> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    Ok(output.status.success())
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

/// Lists local branch names, most recently committed first.
pub fn local_branches(dir: &Path) -> Result<Vec<String>> {
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Lists remote-tracking branches as `(short_name, remote_ref)` pairs, e.g.
/// `("feature", "origin/feature")`, most recently committed first. Skips each
/// remote's symbolic `HEAD` pointer (`origin/HEAD`), which is not a real branch.
pub fn remote_branches(dir: &Path) -> Result<Vec<(String, String)>> {
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/remotes",
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        // `refname:short` is `<remote>/<branch>`; split on the first slash so a
        // branch name that itself contains slashes stays intact.
        .filter_map(|full| {
            full.split_once('/')
                .map(|(_, branch)| (branch.to_string(), full.to_string()))
        })
        .collect())
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

/// Switches the worktree at `dir` to `branch` (an existing local branch).
/// Lets git's own error surface for a dirty tree that would conflict or a
/// branch already checked out in another worktree.
pub fn switch(dir: &Path, branch: &str) -> Result<()> {
    run(dir, &["switch", branch])?;
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

/// Unlocks the worktree at `path`. A locked worktree is refused by both
/// `worktree remove --force` (single force) and `worktree prune`, so unlocking
/// first lets those reclaim it. Returns git's error when it is not locked;
/// callers use this best-effort.
pub fn worktree_unlock(dir: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy();
    run(dir, &["worktree", "unlock", &path_str])?;
    Ok(())
}

/// Prunes worktree admin entries whose directories are gone, so a path can be
/// reused after its directory was removed by hand or via `worktree_remove`.
pub fn worktree_prune(dir: &Path) -> Result<()> {
    run(dir, &["worktree", "prune"])?;
    Ok(())
}

/// Counts commits reachable from `branch` but not from `base`
/// (`git rev-list --count base..branch`). Zero when `branch` has no unique
/// work, e.g. when it equals or is fully merged into `base`.
pub fn commits_ahead_of(dir: &Path, base: &str, branch: &str) -> Result<u32> {
    let range = format!("{base}..{branch}");
    let out = run(dir, &["rev-list", "--count", &range])?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Changed files in the worktree at `dir` (staged, unstaged, and untracked).
/// `--untracked-files=all` expands new directories into their individual files
/// so each one is listed (and viewable) instead of a single collapsed `dir/`.
pub fn status(dir: &Path) -> Result<Vec<StatusEntry>> {
    let out = run(dir, &["status", "--porcelain", "--untracked-files=all"])?;
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

/// Unified diff of a single `path`. Untracked files are diffed against
/// `/dev/null` so their whole contents show as additions.
pub fn diff_file(dir: &Path, path: &str, untracked: bool) -> Result<String> {
    if untracked {
        // `--no-index` exits non-zero when the files differ, which is the
        // normal case here, so read stdout directly instead of via `run`.
        let output = Command::new("git")
            .args(["diff", "--no-index", "--", "/dev/null", path])
            .current_dir(dir)
            .output()?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    } else {
        run(dir, &["diff", "HEAD", "--", path])
    }
}

/// Discards changes to `path`, restoring it to HEAD. Untracked files are
/// removed outright since they have no HEAD version to restore.
pub fn revert_file(dir: &Path, path: &str, untracked: bool) -> Result<()> {
    if untracked {
        run(dir, &["clean", "-fd", "--", path])?;
    } else {
        run(
            dir,
            &[
                "restore",
                "--source=HEAD",
                "--staged",
                "--worktree",
                "--",
                path,
            ],
        )?;
    }
    Ok(())
}

/// One entry from `git stash list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StashEntry {
    pub index: u32,
    /// Full reflog subject, e.g. "WIP on main: 1a2b3c4 fix parser".
    pub message: String,
    /// Branch the stash was taken on.
    pub branch: String,
}

/// One local branch with the metadata shown in `wtm branch list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BranchDetail {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Subject line of the branch's tip commit.
    pub subject: String,
    /// Relative commit date, e.g. "2 days ago".
    pub date: String,
}

/// One commit from `git log`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogEntry {
    pub hash: String,
    pub subject: String,
    pub author: String,
    /// Relative author date, e.g. "3 hours ago".
    pub date: String,
    /// Refs pointing at this commit, e.g. `["HEAD -> main", "origin/main"]`.
    /// Empty for the vast majority of commits.
    pub refs: Vec<String>,
}

/// One line of `git log --graph` output: the ASCII graph drawn to the left,
/// plus the commit on that line. `entry` is `None` for the connector-only lines
/// git emits between commits (`|\`, `|/`, …), which carry art but no commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLine {
    /// The graph art prefix, e.g. `"* "` or `"|\\  "`.
    pub graph: String,
    pub entry: Option<LogEntry>,
}

/// Stages every change in `dir` (`git add -A`).
pub fn stage_all(dir: &Path) -> Result<()> {
    run(dir, &["add", "-A"])?;
    Ok(())
}

/// Stages only `paths` (`git add -- <paths>`).
pub fn stage_paths(dir: &Path, paths: &[String]) -> Result<()> {
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run(dir, &args)?;
    Ok(())
}

/// True when the index holds staged changes ready to commit.
pub fn has_staged_changes(dir: &Path) -> Result<bool> {
    // `diff --cached --quiet` exits non-zero precisely when something is staged.
    Ok(!run_predicate(dir, &["diff", "--cached", "--quiet"])?)
}

/// Commits the staged changes with `message`.
pub fn commit(dir: &Path, message: &str) -> Result<()> {
    run(dir, &["commit", "-m", message])?;
    Ok(())
}

/// Abbreviated hash of HEAD.
pub fn short_hash(dir: &Path) -> Result<String> {
    run(dir, &["rev-parse", "--short", "HEAD"])
}

/// Subject line of the HEAD commit.
pub fn head_subject(dir: &Path) -> Result<String> {
    run(dir, &["log", "-1", "--format=%s"])
}

/// Number of files touched by the HEAD commit. `--root` makes this work for
/// the very first commit, which has no parent to diff against.
pub fn head_files_changed(dir: &Path) -> Result<usize> {
    let out = run(
        dir,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "HEAD",
        ],
    )?;
    Ok(out.lines().filter(|l| !l.is_empty()).count())
}

/// Stashes changes including untracked files (`git stash push -u`).
pub fn stash_push(dir: &Path, message: Option<&str>) -> Result<String> {
    let mut args = vec!["stash", "push", "-u"];
    if let Some(m) = message {
        args.push("-m");
        args.push(m);
    }
    run(dir, &args)
}

/// Stashes only `paths` (including any untracked ones), leaving the rest of
/// the working tree in place.
pub fn stash_push_paths(dir: &Path, paths: &[String], message: Option<&str>) -> Result<String> {
    let mut args = vec!["stash", "push", "--include-untracked"];
    if let Some(m) = message {
        args.push("-m");
        args.push(m);
    }
    args.push("--");
    args.extend(paths.iter().map(String::as_str));
    run(dir, &args)
}

/// Lists stash entries, newest first.
pub fn stash_list(dir: &Path) -> Result<Vec<StashEntry>> {
    // 0x1f (unit separator) keeps the selector and subject apart safely.
    let out = run(dir, &["stash", "list", "--format=%gd\u{1f}%gs"])?;
    Ok(parse_stash_list(&out))
}

/// Runs `git stash <verb>` on an optional specific entry index.
fn stash_op(dir: &Path, verb: &str, index: Option<u32>) -> Result<String> {
    let selector = index.map(|i| format!("stash@{{{i}}}"));
    let mut args = vec!["stash", verb];
    if let Some(s) = selector.as_deref() {
        args.push(s);
    }
    run(dir, &args)
}

/// Outcome of a `git stash pop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashPopStatus {
    /// The stash applied cleanly and was dropped; carries git's output.
    Applied(String),
    /// Applying the stash produced conflicts. The stash was NOT dropped and the
    /// listed files are left with conflict markers to resolve.
    Conflicted(Vec<String>),
}

/// Applies and (on success) drops a stash entry (`git stash pop`). A conflicting
/// pop writes conflict markers, exits non-zero, and deliberately keeps the stash
/// entry; that case is reported as [`StashPopStatus::Conflicted`] rather than an
/// error so a resolver can take over. Any other failure is surfaced as an error.
pub fn stash_pop(dir: &Path, index: Option<u32>) -> Result<StashPopStatus> {
    match stash_op(dir, "pop", index) {
        Ok(out) => Ok(StashPopStatus::Applied(out)),
        Err(e) => {
            // A conflict leaves unmerged index entries behind (and keeps the
            // stash); anything else is a real failure.
            let files = conflicted_files(dir).unwrap_or_default();
            if !files.is_empty() {
                Ok(StashPopStatus::Conflicted(files))
            } else {
                Err(e)
            }
        }
    }
}

/// Applies a stash entry without dropping it (`git stash apply`).
pub fn stash_apply(dir: &Path, index: Option<u32>) -> Result<String> {
    stash_op(dir, "apply", index)
}

/// Drops a stash entry (`git stash drop`).
pub fn stash_drop(dir: &Path, index: Option<u32>) -> Result<String> {
    stash_op(dir, "drop", index)
}

/// True when HEAD has an upstream tracking branch configured.
pub fn has_upstream(dir: &Path) -> bool {
    run_predicate(
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .unwrap_or(false)
}

/// The remote and remote-side branch `branch` tracks, e.g. `("origin",
/// "main")` for a branch tracking `origin/main`. `None` when `branch` has no
/// upstream configured.
///
/// Read from `branch.<name>.remote`/`.merge` rather than `@{upstream}` so it
/// works for any branch, not just the one HEAD is on.
pub fn branch_upstream(dir: &Path, branch: &str) -> Result<Option<(String, String)>> {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let (Ok(remote), Ok(merge)) = (
        run(dir, &["config", "--get", &remote_key]),
        run(dir, &["config", "--get", &merge_key]),
    ) else {
        return Ok(None);
    };
    let remote = remote.trim();
    // `merge` is a full ref (refs/heads/main); the fetch refspec wants the
    // branch name.
    let merge = merge
        .trim()
        .strip_prefix("refs/heads/")
        .unwrap_or(merge.trim());
    if remote.is_empty() || merge.is_empty() {
        return Ok(None);
    }
    Ok(Some((remote.to_string(), merge.to_string())))
}

/// Fast-forwards the local `branch` to `remote`'s `src` branch without checking
/// it out, by fetching straight into the local ref. git refuses the update when
/// it would not be a fast-forward, which is exactly the guarantee we want, and
/// also refuses when `branch` is checked out anywhere (callers pull there
/// instead).
pub fn fetch_into_branch(dir: &Path, remote: &str, src: &str, branch: &str) -> Result<String> {
    let refspec = format!("{src}:{branch}");
    run(dir, &["fetch", remote, &refspec])
}

/// Pulls the current branch. Fast-forward only by default; `rebase` rebases
/// local commits onto the upstream instead.
pub fn pull(dir: &Path, rebase: bool) -> Result<String> {
    let mode = if rebase { "--rebase" } else { "--ff-only" };
    run(dir, &["pull", mode])
}

/// Pushes the current branch to its existing upstream.
pub fn push(dir: &Path, force_with_lease: bool) -> Result<String> {
    let mut args = vec!["push"];
    if force_with_lease {
        args.push("--force-with-lease");
    }
    run(dir, &args)
}

/// Pushes `branch` to `remote` and records it as the upstream (`push -u`).
pub fn push_set_upstream(
    dir: &Path,
    remote: &str,
    branch: &str,
    force_with_lease: bool,
) -> Result<String> {
    let mut args = vec!["push"];
    if force_with_lease {
        args.push("--force-with-lease");
    }
    args.push("-u");
    args.push(remote);
    args.push(branch);
    run(dir, &args)
}

/// Names of the repository's configured remotes.
pub fn remotes(dir: &Path) -> Result<Vec<String>> {
    let out = run(dir, &["remote"])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Fetches every remote and prunes deleted remote branches.
pub fn fetch_all_prune(dir: &Path) -> Result<String> {
    run(dir, &["fetch", "--all", "--prune"])
}

/// Creates branch `name` (optionally starting at `from`) without a worktree.
pub fn branch_create(dir: &Path, name: &str, from: Option<&str>) -> Result<()> {
    let mut args = vec!["branch", name];
    if let Some(f) = from {
        args.push(f);
    }
    run(dir, &args)?;
    Ok(())
}

/// Deletes branch `name`; `force` uses `-D` (delete even if unmerged).
pub fn branch_delete_flag(dir: &Path, name: &str, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    run(dir, &["branch", flag, name])?;
    Ok(())
}

/// True when `err` is git's "not fully merged" refusal from a safe (`-d`)
/// branch delete, as opposed to any other failure. Lets callers offer a force
/// (`-D`) retry only for that specific, recoverable case.
pub fn is_not_merged_error(err: &GitError) -> bool {
    matches!(err, GitError::Command { stderr, .. } if stderr.contains("not fully merged"))
}

/// Best-effort name of the repository's default branch, used when a worktree
/// must be moved off a branch that is about to be deleted. Prefers what
/// `origin/HEAD` points at, then a local `main`, then `master`, then the first
/// local branch. Errors only when the repo has no local branches at all.
pub fn default_branch(dir: &Path) -> Result<String> {
    if let Ok(head) = run(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let name = head.strip_prefix("origin/").unwrap_or(&head).to_string();
        if !name.is_empty() {
            return Ok(name);
        }
    }
    if branch_exists(dir, "main") {
        return Ok("main".to_string());
    }
    if branch_exists(dir, "master") {
        return Ok("master".to_string());
    }
    local_branches(dir)?
        .into_iter()
        .next()
        .ok_or_else(|| GitError::Command {
            args: "for-each-ref refs/heads".to_string(),
            stderr: "repository has no local branches".to_string(),
        })
}

/// Renames branch `old` to `new` (`git branch -m`).
pub fn branch_rename(dir: &Path, old: &str, new: &str) -> Result<()> {
    run(dir, &["branch", "-m", old, new])?;
    Ok(())
}

/// Lists local branches with upstream tracking, tip subject, and date,
/// most recently committed first.
pub fn branch_details(dir: &Path) -> Result<Vec<BranchDetail>> {
    // Fields are separated by 0x1f so subjects containing spaces stay intact.
    let format = "--format=%(refname:short)\u{1f}%(upstream:short)\u{1f}\
                  %(upstream:track)\u{1f}%(contents:subject)\u{1f}%(committerdate:relative)";
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            format,
            "refs/heads",
        ],
    )?;
    Ok(parse_branch_details(&out))
}

/// `git log` pretty format for [`parse_log_line`], with `%h` (abbreviated) or
/// `%H` (full) substituted in for the hash.
fn log_format(full_hash: bool) -> &'static str {
    if full_hash {
        "%H\u{1f}%s\u{1f}%an\u{1f}%ad\u{1f}%D"
    } else {
        "%h\u{1f}%s\u{1f}%an\u{1f}%ad\u{1f}%D"
    }
}

/// Recent commits reachable from HEAD in `dir` (newest first).
pub fn log(dir: &Path, count: u32) -> Result<Vec<LogEntry>> {
    let count = count.to_string();
    let format = format!("--format={}", log_format(false));
    let out = run(dir, &["log", "-n", &count, "--date=relative", &format])?;
    Ok(parse_log(&out))
}

/// Recent commits reachable from an arbitrary ref (branch, tag, hash), newest
/// first. Runs in `dir` (typically the repo root) so any local branch can be
/// inspected without checking it out. Full (`%H`) hashes are used so the
/// results can be passed straight to `cherry_pick`.
pub fn log_ref(dir: &Path, refname: &str, count: u32) -> Result<Vec<LogEntry>> {
    let count = count.to_string();
    let format = format!("--format={}", log_format(true));
    let out = run(
        dir,
        &["log", refname, "-n", &count, "--date=relative", &format],
    )?;
    Ok(parse_log(&out))
}

/// The same history as [`log_ref`], but rendered by `git log --graph` so the
/// branch/merge topology comes back drawn. `refname` of `None` graphs HEAD.
///
/// Letting git draw the art (rather than assigning lanes ourselves) keeps the
/// topology exactly right; a NUL byte leads the pretty format so each line
/// splits cleanly into "graph art" and "commit fields".
pub fn log_graph(
    dir: &Path,
    refname: Option<&str>,
    count: u32,
    full_hash: bool,
) -> Result<Vec<GraphLine>> {
    let count = count.to_string();
    let format = format!("--format=%x00{}", log_format(full_hash));
    let mut args = vec!["log", "--graph", "-n", &count, "--date=relative", &format];
    if let Some(refname) = refname {
        args.push(refname);
    }
    let out = run(dir, &args)?;
    Ok(parse_log_graph(&out))
}

/// Outcome of a `git cherry-pick` sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CherryPickStatus {
    /// Every commit applied (committed, or staged under `no_commit`).
    Applied,
    /// A commit conflicted; the cherry-pick is left in progress with the listed
    /// files in conflict, so a resolver can finish it.
    Conflicted(Vec<String>),
}

/// Cherry-picks `commits` onto the branch checked out in `dir`. `commits` must
/// be ordered oldest-first, the order git applies them. When `no_commit` is
/// true (`-n`) the changes land staged in the working tree without a commit, so
/// the caller can review or edit before committing; otherwise each commit is
/// recorded with its original message. A conflict is reported as
/// [`CherryPickStatus::Conflicted`] and deliberately leaves the sequence in
/// progress (conflict markers in the files) so a resolver can take over; use
/// `cherry_pick_abort` or `cherry_pick_continue` to finish. Any other failure
/// is surfaced as an error after cleaning up the half-applied sequence.
pub fn cherry_pick(dir: &Path, commits: &[String], no_commit: bool) -> Result<CherryPickStatus> {
    let mut args: Vec<&str> = vec!["cherry-pick"];
    if no_commit {
        args.push("-n");
    }
    args.extend(commits.iter().map(String::as_str));
    match run(dir, &args) {
        Ok(_) => Ok(CherryPickStatus::Applied),
        Err(e) => {
            // A genuine conflict leaves unmerged index entries behind; report it
            // and leave the sequence in progress for the resolver. Anything else
            // is a real failure, so clean up rather than leaving a mess.
            let files = conflicted_files(dir).unwrap_or_default();
            if !files.is_empty() {
                Ok(CherryPickStatus::Conflicted(files))
            } else {
                let _ = run(dir, &["cherry-pick", "--abort"]);
                Err(e)
            }
        }
    }
}

/// True while a cherry-pick is in progress in `dir` (CHERRY_PICK_HEAD exists).
pub fn is_cherry_picking(dir: &Path) -> bool {
    run_predicate(
        dir,
        &["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"],
    )
    .unwrap_or(false)
}

/// Continues an in-progress cherry-pick once its conflicts are resolved and
/// staged, recording the commit with git's prepared message. `core.editor=true`
/// suppresses the editor so the message is accepted non-interactively.
pub fn cherry_pick_continue(dir: &Path) -> Result<()> {
    run(
        dir,
        &["-c", "core.editor=true", "cherry-pick", "--continue"],
    )?;
    Ok(())
}

/// Aborts an in-progress cherry-pick, restoring the pre-cherry-pick state.
pub fn cherry_pick_abort(dir: &Path) -> Result<()> {
    run(dir, &["cherry-pick", "--abort"])?;
    Ok(())
}

/// Discards all tracked changes in the working tree and index, resetting to
/// HEAD. Used to undo a conflicting stash pop's application while leaving the
/// (still-present) stash entry intact.
pub fn reset_hard(dir: &Path) -> Result<()> {
    run(dir, &["reset", "--hard", "HEAD"])?;
    Ok(())
}

/// Outcome of a `git merge` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStatus {
    /// The current branch already contained the source; nothing changed.
    AlreadyUpToDate,
    /// The merge completed, either by fast-forward or a new merge commit.
    Merged,
    /// The merge stopped on conflicts; the paths of the conflicted files.
    Conflicted(Vec<String>),
}

/// Merges `source_ref` into the branch checked out in `dir`. `no_ff` forces a
/// merge commit even when a fast-forward would do. On a conflict the merge is
/// deliberately left in progress (MERGE_HEAD present, conflict markers in the
/// files) so a resolver can take over; use `merge_abort` or `merge_continue`
/// to finish. Any other failure (dirty tree, unknown ref) is surfaced as an
/// error.
pub fn merge(dir: &Path, source_ref: &str, no_ff: bool) -> Result<MergeStatus> {
    let mut args = vec!["merge"];
    if no_ff {
        args.push("--no-ff");
    }
    args.push(source_ref);
    match run(dir, &args) {
        Ok(out) => {
            if out.contains("Already up to date") {
                Ok(MergeStatus::AlreadyUpToDate)
            } else {
                Ok(MergeStatus::Merged)
            }
        }
        Err(e) => {
            // A genuine conflict exits non-zero but leaves MERGE_HEAD and
            // unmerged index entries behind; anything else is a real failure.
            let files = conflicted_files(dir).unwrap_or_default();
            if is_merging(dir) && !files.is_empty() {
                Ok(MergeStatus::Conflicted(files))
            } else {
                Err(e)
            }
        }
    }
}

/// Paths currently in an unmerged (conflict) state, i.e. porcelain codes
/// UU, AA, DD, AU, UA, DU, or UD.
pub fn conflicted_files(dir: &Path) -> Result<Vec<String>> {
    const CONFLICT_CODES: [&str; 7] = ["UU", "AA", "DD", "AU", "UA", "DU", "UD"];
    let out = run(dir, &["status", "--porcelain"])?;
    Ok(parse_status_porcelain(&out)
        .into_iter()
        .filter(|e| CONFLICT_CODES.contains(&e.code.as_str()))
        .map(|e| e.path)
        .collect())
}

/// True while a merge is in progress in `dir` (MERGE_HEAD exists).
pub fn is_merging(dir: &Path) -> bool {
    run_predicate(dir, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).unwrap_or(false)
}

/// Aborts an in-progress merge, restoring the pre-merge state.
pub fn merge_abort(dir: &Path) -> Result<()> {
    run(dir, &["merge", "--abort"])?;
    Ok(())
}

/// Commits an in-progress merge once its conflicts are resolved and staged,
/// keeping the merge message git prepared (`--no-edit`).
pub fn merge_continue(dir: &Path) -> Result<()> {
    run(dir, &["commit", "--no-edit"])?;
    Ok(())
}

/// Resolves a conflicted path by taking one whole side: `ours` selects
/// `--ours`, otherwise `--theirs`. Leaves the result unstaged; pair with
/// `stage_paths` to mark it resolved.
pub fn checkout_conflict_side(dir: &Path, path: &str, ours: bool) -> Result<()> {
    let flag = if ours { "--ours" } else { "--theirs" };
    run(dir, &["checkout", flag, "--", path])?;
    Ok(())
}

/// Finds a remote-tracking ref matching `branch` (e.g. "origin/feature"),
/// searching each configured remote. Returns the short `<remote>/<branch>`
/// form for use as a branch base.
pub fn find_remote_ref(dir: &Path, branch: &str) -> Result<Option<String>> {
    for remote in remotes(dir)? {
        let refname = format!("refs/remotes/{remote}/{branch}");
        if run_predicate(dir, &["show-ref", "--verify", "--quiet", &refname])? {
            return Ok(Some(format!("{remote}/{branch}")));
        }
    }
    Ok(None)
}

/// Parses `git stash list` output formatted as `<selector>\x1f<subject>`.
pub fn parse_stash_list(out: &str) -> Vec<StashEntry> {
    out.lines()
        .filter_map(|line| {
            let (selector, subject) = line.split_once('\u{1f}')?;
            let index = selector
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(StashEntry {
                index,
                message: subject.to_string(),
                branch: parse_stash_branch(subject),
            })
        })
        .collect()
}

/// Extracts the branch name from a stash reflog subject like
/// "WIP on main: ..." or "On main: my message".
fn parse_stash_branch(subject: &str) -> String {
    let rest = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "))
        .unwrap_or(subject);
    match rest.split_once(':') {
        Some((branch, _)) => branch.to_string(),
        None => String::new(),
    }
}

/// Parses `branch_details` output (one branch per line, 0x1f-separated fields).
pub fn parse_branch_details(out: &str) -> Vec<BranchDetail> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split('\u{1f}');
            let name = fields.next()?.to_string();
            let upstream = fields.next().unwrap_or("");
            let track = fields.next().unwrap_or("");
            let subject = fields.next().unwrap_or("").to_string();
            let date = fields.next().unwrap_or("").to_string();
            let (ahead, behind) = parse_track(track);
            Some(BranchDetail {
                name,
                upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
                ahead,
                behind,
                subject,
                date,
            })
        })
        .collect()
}

/// Parses a `%(upstream:track)` value such as "[ahead 1, behind 2]",
/// "[ahead 3]", "[gone]", or "" into ahead/behind counts.
fn parse_track(track: &str) -> (u32, u32) {
    let mut ahead = 0;
    let mut behind = 0;
    for part in track.trim_matches(['[', ']']).split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// Parses one `<hash>\x1f<subject>\x1f<author>\x1f<date>\x1f<refs>` record.
/// Returns `None` for a line with no hash.
fn parse_log_line(line: &str) -> Option<LogEntry> {
    let mut fields = line.split('\u{1f}');
    let hash = fields.next()?.to_string();
    if hash.is_empty() {
        return None;
    }
    let subject = fields.next().unwrap_or("").to_string();
    let author = fields.next().unwrap_or("").to_string();
    let date = fields.next().unwrap_or("").to_string();
    // `%D` is a comma-separated list ("HEAD -> main, origin/main"), empty for
    // commits no ref points at.
    let refs = fields
        .next()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect();
    Some(LogEntry {
        hash,
        subject,
        author,
        date,
        refs,
    })
}

/// Parses `git log` output formatted by [`log_format`].
pub fn parse_log(out: &str) -> Vec<LogEntry> {
    out.lines().filter_map(parse_log_line).collect()
}

/// Parses `git log --graph` output whose pretty format starts with a NUL.
/// Lines containing a NUL are commits (art before it, fields after); the rest
/// are connector-only art. Blank lines are dropped, but art-only lines are kept
/// since they carry the branch/merge structure.
pub fn parse_log_graph(out: &str) -> Vec<GraphLine> {
    out.lines()
        .filter_map(|line| match line.split_once('\u{0}') {
            Some((graph, data)) => Some(GraphLine {
                graph: graph.to_string(),
                entry: parse_log_line(data),
            }),
            None => {
                let graph = line.trim_end();
                (!graph.trim().is_empty()).then(|| GraphLine {
                    graph: graph.to_string(),
                    entry: None,
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Builds a throwaway git repo with a single commit on `main` and no
    /// remotes, returning its temp dir and path.
    fn temp_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@e.st"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        (tmp, repo)
    }

    #[test]
    fn default_branch_falls_back_to_initial_branch_without_origin() {
        let (_tmp, repo) = temp_repo();
        // No origin remote, but a local `main`, so it should report "main".
        assert_eq!(default_branch(&repo).unwrap(), "main");
    }

    #[test]
    fn remote_branches_lists_short_names_and_skips_head() {
        let (_tmp, repo) = temp_repo();
        let sha = run(&repo, &["rev-parse", "HEAD"]).unwrap();
        let sha = sha.trim();
        // Simulate fetched remote-tracking refs, including a slashed name and
        // the symbolic HEAD that must be filtered out.
        for refname in [
            "refs/remotes/origin/teammate",
            "refs/remotes/origin/feature/x",
        ] {
            run(&repo, &["update-ref", refname, sha]).unwrap();
        }
        run(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/teammate",
            ],
        )
        .unwrap();
        let mut got = remote_branches(&repo).unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("feature/x".to_string(), "origin/feature/x".to_string()),
                ("teammate".to_string(), "origin/teammate".to_string()),
            ]
        );
    }

    #[test]
    fn is_not_merged_error_matches_only_the_merge_refusal() {
        let merge = GitError::Command {
            args: "branch -d x".to_string(),
            stderr: "error: the branch 'x' is not fully merged".to_string(),
        };
        let other = GitError::Command {
            args: "branch -d x".to_string(),
            stderr: "error: branch 'x' not found".to_string(),
        };
        assert!(is_not_merged_error(&merge));
        assert!(!is_not_merged_error(&other));
    }

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
        assert!(parse_stash_list("").is_empty());
        assert!(parse_branch_details("").is_empty());
        assert!(parse_log("").is_empty());
    }

    #[test]
    fn parses_stash_list_entries() {
        let out = "stash@{0}\u{1f}WIP on main: 1a2b3c4 fix parser\n\
                   stash@{1}\u{1f}On feature/login: my saved work\n";
        let entries = parse_stash_list(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].index, 0);
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "WIP on main: 1a2b3c4 fix parser");
        assert_eq!(entries[1].index, 1);
        assert_eq!(entries[1].branch, "feature/login");
    }

    #[test]
    fn parses_track_variants() {
        assert_eq!(parse_track("[ahead 1, behind 2]"), (1, 2));
        assert_eq!(parse_track("[ahead 3]"), (3, 0));
        assert_eq!(parse_track("[behind 4]"), (0, 4));
        assert_eq!(parse_track("[gone]"), (0, 0));
        assert_eq!(parse_track(""), (0, 0));
    }

    #[test]
    fn parses_branch_details_with_and_without_upstream() {
        let out = "main\u{1f}origin/main\u{1f}[ahead 1, behind 2]\u{1f}latest work\u{1f}2 days ago\n\
                   feature\u{1f}\u{1f}\u{1f}wip\u{1f}5 minutes ago\n";
        let details = parse_branch_details(out);
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].name, "main");
        assert_eq!(details[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(details[0].ahead, 1);
        assert_eq!(details[0].behind, 2);
        assert_eq!(details[0].subject, "latest work");
        assert_eq!(details[0].date, "2 days ago");
        assert_eq!(details[1].name, "feature");
        assert_eq!(details[1].upstream, None);
        assert_eq!(details[1].ahead, 0);
    }

    #[test]
    fn parses_log_entries() {
        let out = "1a2b3c4\u{1f}fix parser\u{1f}Ada\u{1f}3 hours ago\n\
                   5d6e7f8\u{1f}add tests\u{1f}Grace\u{1f}yesterday\n";
        let entries = parse_log(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, "1a2b3c4");
        assert_eq!(entries[0].subject, "fix parser");
        assert_eq!(entries[0].author, "Ada");
        assert_eq!(entries[0].date, "3 hours ago");
        assert_eq!(entries[1].hash, "5d6e7f8");
    }

    #[test]
    fn parses_log_ref_decorations() {
        let out = "1a2b3c4\u{1f}fix parser\u{1f}Ada\u{1f}3 hours ago\u{1f}HEAD -> main, origin/main\n\
                   5d6e7f8\u{1f}add tests\u{1f}Grace\u{1f}yesterday\u{1f}\n";
        let entries = parse_log(out);
        assert_eq!(entries[0].refs, vec!["HEAD -> main", "origin/main"]);
        // A commit no ref points at decorates to nothing, not to one empty ref.
        assert!(entries[1].refs.is_empty());
    }

    #[test]
    fn parses_graph_art_and_commit_lines() {
        let out = "* \u{0}1a2b3c4\u{1f}merge feature\u{1f}Ada\u{1f}1 hour ago\u{1f}HEAD -> main\n\
                   |\\  \n\
                   | * \u{0}5d6e7f8\u{1f}add tests\u{1f}Grace\u{1f}2 hours ago\u{1f}\n\
                   |/  \n\
                   * \u{0}9a8b7c6\u{1f}init\u{1f}Ada\u{1f}3 hours ago\u{1f}\n";
        let lines = parse_log_graph(out);
        assert_eq!(lines.len(), 5);
        // Commit rows keep their art and their fields.
        assert_eq!(lines[0].graph, "* ");
        assert_eq!(lines[0].entry.as_ref().unwrap().subject, "merge feature");
        assert_eq!(lines[0].entry.as_ref().unwrap().refs, vec!["HEAD -> main"]);
        // Art-only rows carry structure but no commit.
        assert_eq!(lines[1].graph, "|\\");
        assert!(lines[1].entry.is_none());
        assert_eq!(lines[2].entry.as_ref().unwrap().hash, "5d6e7f8");
        assert!(lines[3].entry.is_none());
        assert_eq!(lines[4].entry.as_ref().unwrap().subject, "init");
    }

    #[test]
    fn parses_empty_graph_output() {
        assert!(parse_log_graph("").is_empty());
        // Blank lines are dropped rather than becoming empty art rows.
        assert!(parse_log_graph("\n  \n").is_empty());
    }

    /// The real `git log --graph` on a merge: the art must come back drawn, and
    /// every commit must still be readable off it.
    #[test]
    fn log_graph_draws_a_merge() {
        let (_tmp, repo) = temp_repo();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["checkout", "-b", "feature"]);
        git(&["commit", "--allow-empty", "-m", "feature work"]);
        git(&["checkout", "main"]);
        git(&["commit", "--allow-empty", "-m", "main work"]);
        git(&["merge", "--no-ff", "feature", "-m", "merge feature"]);

        let lines = log_graph(&repo, Some("main"), 20, false).unwrap();
        let subjects: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.entry.as_ref())
            .map(|e| e.subject.as_str())
            .collect();
        // `--graph` implies `--topo-order`, so the merged-in branch is drawn as
        // one unbroken run rather than interleaved by date with `main work`.
        assert_eq!(
            subjects,
            ["merge feature", "feature work", "main work", "init"]
        );
        // A merge forces git to draw connector rows; without them the art would
        // be meaningless.
        assert!(
            lines.iter().any(|l| l.entry.is_none()),
            "expected art-only connector rows, got {lines:#?}"
        );
        // The merge commit sits on a lane, and a second lane exists beside it.
        assert!(lines.iter().any(|l| l.graph.contains('*')));
        assert!(lines.iter().any(|l| l.graph.contains('|')));
    }

    #[test]
    fn branch_upstream_is_none_without_tracking() {
        let (_tmp, repo) = temp_repo();
        assert_eq!(branch_upstream(&repo, "main").unwrap(), None);
    }

    /// A branch tracking a remote reports the remote and the remote-side branch
    /// name, with `refs/heads/` stripped so it can go straight into a refspec.
    #[test]
    fn branch_upstream_reads_tracking_config() {
        let (_tmp, repo) = temp_repo();
        for args in [
            vec!["config", "branch.main.remote", "origin"],
            vec!["config", "branch.main.merge", "refs/heads/trunk"],
        ] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        assert_eq!(
            branch_upstream(&repo, "main").unwrap(),
            Some(("origin".to_string(), "trunk".to_string()))
        );
    }
}
