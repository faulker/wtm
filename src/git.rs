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

/// Applies and drops a stash entry (`git stash pop`).
pub fn stash_pop(dir: &Path, index: Option<u32>) -> Result<String> {
    stash_op(dir, "pop", index)
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
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
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

/// Recent commits reachable from HEAD in `dir` (newest first).
pub fn log(dir: &Path, count: u32) -> Result<Vec<LogEntry>> {
    let count = count.to_string();
    let out = run(
        dir,
        &[
            "log",
            "-n",
            &count,
            "--date=relative",
            "--format=%h\u{1f}%s\u{1f}%an\u{1f}%ad",
        ],
    )?;
    Ok(parse_log(&out))
}

/// Recent commits reachable from an arbitrary ref (branch, tag, hash), newest
/// first. Runs in `dir` (typically the repo root) so any local branch can be
/// inspected without checking it out. Full (`%H`) hashes are used so the
/// results can be passed straight to `cherry_pick`.
pub fn log_ref(dir: &Path, refname: &str, count: u32) -> Result<Vec<LogEntry>> {
    let count = count.to_string();
    let out = run(
        dir,
        &[
            "log",
            refname,
            "-n",
            &count,
            "--date=relative",
            "--format=%H\u{1f}%s\u{1f}%an\u{1f}%ad",
        ],
    )?;
    Ok(parse_log(&out))
}

/// Cherry-picks `commits` onto the branch checked out in `dir`. `commits` must
/// be ordered oldest-first, the order git applies them. When `no_commit` is
/// true (`-x -n`) the changes land staged in the working tree without a commit,
/// so the caller can review or edit before committing; otherwise each commit is
/// recorded with its original message. On failure the in-progress cherry-pick
/// is aborted so the worktree is left clean rather than mid-sequence.
pub fn cherry_pick(dir: &Path, commits: &[String], no_commit: bool) -> Result<()> {
    let mut args: Vec<&str> = vec!["cherry-pick"];
    if no_commit {
        args.push("-n");
    }
    args.extend(commits.iter().map(String::as_str));
    match run(dir, &args) {
        Ok(_) => Ok(()),
        Err(e) => {
            // Best-effort cleanup: leave no half-applied sequence or -n changes
            // behind, so the target worktree stays usable after a conflict.
            let _ = run(dir, &["cherry-pick", "--abort"]);
            Err(e)
        }
    }
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

/// Parses `git log` output formatted as `<hash>\x1f<subject>\x1f<author>\x1f<date>`.
pub fn parse_log(out: &str) -> Vec<LogEntry> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split('\u{1f}');
            Some(LogEntry {
                hash: fields.next()?.to_string(),
                subject: fields.next().unwrap_or("").to_string(),
                author: fields.next().unwrap_or("").to_string(),
                date: fields.next().unwrap_or("").to_string(),
            })
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
}
