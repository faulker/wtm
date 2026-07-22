//! Rendering of command results as human-readable text or JSON.

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::conflict::ConflictSegment;
use crate::git::StatusEntry;
use crate::ops::{
    BranchCreateResult, BranchDeleteResult, BranchListResult, BranchRenameResult,
    CherryPickOutcome, CommitResult, CompleteResolutionResult, ConflictFile, CreateResult,
    FetchResult, LogResult, MergeOutcome, PullResult, PushResult, StashListResult, StashPopOutcome,
    StashResult, SwitchResult, WorktreeInfo, WorktreeRenameResult,
};

/// Serializes `value` as pretty JSON to stdout.
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Human-readable worktree table.
pub fn print_list(infos: &[WorktreeInfo]) {
    if infos.is_empty() {
        println!("no worktrees found");
        return;
    }
    // The name column must fit the `*` marker appended to the main worktree.
    let name_w = infos
        .iter()
        .map(|i| i.name.len() + usize::from(i.is_main))
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<name_w$}  {:<10}  {:<10}  PATH",
        "NAME", "DIRTY", "UPSTREAM"
    );
    for info in infos {
        let dirty = if info.dirty > 0 {
            format!("{} file(s)", info.dirty)
        } else {
            "clean".to_string()
        };
        let upstream = match info.ahead_behind {
            Some(ab) => format!("+{} -{}", ab.ahead, ab.behind),
            None => "-".to_string(),
        };
        let mut name = info.name.clone();
        if info.is_main {
            name.push('*');
        }
        println!(
            "{name:<name_w$}  {dirty:<10}  {upstream:<10}  {}",
            info.path
        );
    }
}

/// Human-readable create report, including each setup step's outcome.
pub fn print_create(result: &CreateResult) {
    let action = if result.created_branch {
        "created branch"
    } else {
        "checked out"
    };
    println!(
        "worktree ready: {} ({action} '{}')",
        result.path, result.branch
    );
    if let Some(remote) = &result.tracked_remote {
        println!("  tracking remote branch {remote}");
    }
    for step in &result.setup {
        let mark = if step.ok { "ok" } else { "FAILED" };
        match &step.detail {
            Some(detail) => println!("  [{mark}] {} ({detail})", step.step),
            None => println!("  [{mark}] {}", step.step),
        }
    }
    if !result.setup_ok {
        println!("warning: some setup steps failed; the worktree was kept");
    }
}

/// Human-readable status listing (porcelain code + path per line).
pub fn print_status(info: &WorktreeInfo, entries: &[StatusEntry]) {
    if entries.is_empty() {
        println!("{}: clean", info.name);
        return;
    }
    println!("{}: {} change(s)", info.name, entries.len());
    for e in entries {
        println!("  {} {}", e.code, e.path);
    }
}

/// JSON envelope for status output.
pub fn status_json(info: &WorktreeInfo, entries: &[StatusEntry]) -> serde_json::Value {
    json!({ "worktree": info, "changes": entries })
}

/// JSON envelope for diff output.
pub fn diff_json(info: &WorktreeInfo, diff: &str) -> serde_json::Value {
    json!({ "worktree": info, "diff": diff })
}

/// JSON envelope for remove output.
pub fn remove_json(info: &WorktreeInfo, deleted_branch: bool) -> serde_json::Value {
    json!({ "removed": info, "deleted_branch": deleted_branch })
}

/// Human-readable commit confirmation.
pub fn print_commit(result: &CommitResult) {
    println!(
        "[{}] {} ({} file(s) changed)",
        result.hash, result.summary, result.files_changed
    );
}

/// Human-readable stash action confirmation.
pub fn print_stash(result: &StashResult) {
    // git's own output is the friendliest summary of what happened.
    if result.output.is_empty() {
        println!("stash {}: done", result.action);
    } else {
        println!("{}", result.output);
    }
}

/// Human-readable stash listing.
pub fn print_stash_list(result: &StashListResult) {
    if result.entries.is_empty() {
        println!("{}: no stash entries", result.name);
        return;
    }
    for entry in &result.entries {
        println!(
            "  stash@{{{}}}  ({})  {}",
            entry.index, entry.branch, entry.message
        );
    }
}

/// Human-readable pull result.
pub fn print_pull(result: &PullResult) {
    if result.already_up_to_date {
        println!("{}: already up to date", result.name);
    } else {
        match result.ahead_behind {
            Some(ab) => println!(
                "{}: pulled (now +{} -{} vs upstream)",
                result.name, ab.ahead, ab.behind
            ),
            None => println!("{}: pulled", result.name),
        }
    }
}

/// Human-readable push result.
pub fn print_push(result: &PushResult) {
    match (&result.set_upstream, &result.remote) {
        (true, Some(remote)) => println!(
            "{}: pushed '{}' and set upstream to {}/{}",
            result.name, result.branch, remote, result.branch
        ),
        _ => println!("{}: pushed '{}'", result.name, result.branch),
    }
}

/// Human-readable fetch result.
pub fn print_fetch(result: &FetchResult) {
    if result.remotes.is_empty() {
        println!("fetched (no remotes configured)");
    } else {
        println!("fetched remotes: {}", result.remotes.join(", "));
    }
}

/// Human-readable switch confirmation.
pub fn print_switch(result: &SwitchResult) {
    println!("switched '{}' to '{}'", result.name, result.branch);
}

/// Human-readable branch listing.
pub fn print_branch_list(result: &BranchListResult) {
    if result.branches.is_empty() {
        println!("no local branches");
        return;
    }
    let name_w = result
        .branches
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<name_w$}  {:<10}  {:<10}  {:<7}  LAST COMMIT",
        "NAME", "CHECKOUT", "UPSTREAM", "FLAGS"
    );
    for b in &result.branches {
        let checkout = if b.checked_out_path.is_some() {
            "worktree"
        } else {
            "-"
        };
        let upstream = if b.upstream.is_some() {
            format!("+{} -{}", b.ahead, b.behind)
        } else {
            "-".to_string()
        };
        let flags = if b.merged { "merged" } else { "-" };
        println!(
            "{:<name_w$}  {checkout:<10}  {upstream:<10}  {flags:<7}  {} ({})",
            b.name, b.subject, b.date
        );
    }
}

/// Human-readable branch-create confirmation.
pub fn print_branch_create(result: &BranchCreateResult) {
    println!("created branch '{}' from {}", result.name, result.from);
}

/// Human-readable branch-delete confirmation.
pub fn print_branch_delete(result: &BranchDeleteResult) {
    let how = if result.forced { " (forced)" } else { "" };
    println!("deleted branch '{}'{how}", result.name);
}

/// Human-readable branch-rename confirmation.
pub fn print_branch_rename(result: &BranchRenameResult) {
    println!("renamed branch '{}' to '{}'", result.old, result.new);
}

/// Human-readable worktree-rename confirmation.
pub fn print_worktree_rename(result: &WorktreeRenameResult) {
    println!(
        "renamed worktree '{}' to '{}' ({} → {})",
        result.old_name, result.new_name, result.old_path, result.new_path
    );
}

/// Human-readable cherry-pick outcome.
pub fn print_cherry_pick(result: &CherryPickOutcome) {
    match result {
        CherryPickOutcome::Applied {
            target,
            count,
            committed,
        } => {
            if *committed {
                println!("cherry-picked {count} commit(s) into '{target}'");
            } else {
                println!("loaded {count} commit(s) into '{target}' (review, then commit)");
            }
        }
        CherryPickOutcome::Conflicted { target, files } => {
            println!(
                "{target}: cherry-pick stopped on {} conflicted file(s):",
                files.len()
            );
            for f in files {
                println!("  {f}");
            }
            println!(
                "resolve each with `wtm resolve {target} <file> --ours|--theirs|--both`, \
                 then `wtm merge --into {target} --continue`"
            );
        }
    }
}

/// Human-readable stash-pop outcome.
pub fn print_stash_pop(result: &StashPopOutcome) {
    match result {
        StashPopOutcome::Applied { output, .. } => {
            if output.is_empty() {
                println!("stash pop: done");
            } else {
                println!("{output}");
            }
        }
        StashPopOutcome::Conflicted { name, index, files } => {
            println!(
                "{name}: stash pop stopped on {} conflicted file(s):",
                files.len()
            );
            for f in files {
                println!("  {f}");
            }
            // The conflicting pop kept the stash; finishing means dropping it.
            let drop_cmd = match index {
                Some(i) => format!("wtm stash drop {name} --index {i}"),
                None => format!("wtm stash drop {name}"),
            };
            println!(
                "resolve each with `wtm resolve {name} <file> --ours|--theirs|--both`, \
                 then `{drop_cmd}` to finish"
            );
        }
    }
}

/// Human-readable merge/update outcome for the worktree named `target`.
pub fn print_merge_outcome(target: &str, result: &MergeOutcome) {
    match result {
        MergeOutcome::UpToDate => println!("{target}: already up to date"),
        MergeOutcome::Clean { commit } => println!("{target}: merged ({commit})"),
        MergeOutcome::Conflicted { files } => {
            println!(
                "{target}: merge stopped on {} conflicted file(s):",
                files.len()
            );
            for f in files {
                println!("  {f}");
            }
            println!(
                "resolve each with `wtm resolve {target} <file> --ours|--theirs|--both`, \
                 then `wtm merge --into {target} --continue`"
            );
        }
    }
}

/// JSON envelope for `conflicts` output when listing files (no file argument).
pub fn conflicts_json(target: &str, files: &[String]) -> serde_json::Value {
    json!({ "target": target, "files": files })
}

/// Human-readable list of a worktree's conflicted files.
pub fn print_conflicts(target: &str, files: &[String]) {
    if files.is_empty() {
        println!("{target}: no conflicts");
        return;
    }
    println!("{target}: {} conflicted file(s)", files.len());
    for f in files {
        println!("  {f}");
    }
}

/// Human-readable view of one conflicted file's hunks. Agents should prefer
/// `--json` for the structured segments.
pub fn print_conflict_file(file: &ConflictFile) {
    println!(
        "{} (ours: {}, theirs: {})",
        file.path, file.ours_label, file.theirs_label
    );
    let mut hunk_no = 0;
    for segment in &file.segments {
        if let ConflictSegment::Hunk { ours, theirs, .. } = segment {
            hunk_no += 1;
            println!("--- hunk {hunk_no} ---");
            println!("<<<<<<< {}", file.ours_label);
            print!("{ours}");
            println!("=======");
            print!("{theirs}");
            println!(">>>>>>> {}", file.theirs_label);
        }
    }
}

/// JSON envelope for `resolve` output.
pub fn resolve_json(target: &str, file: &str, action: &str) -> serde_json::Value {
    json!({ "target": target, "file": file, "action": action })
}

/// Human-readable resolve confirmation.
pub fn print_resolve(target: &str, file: &str, action: &str) {
    println!("{target}: resolved {file} ({action})");
}

/// Human-readable resolution-complete confirmation.
pub fn print_complete_resolution(result: &CompleteResolutionResult) {
    match &result.commit {
        Some(commit) => println!("{}: resolution completed ({commit})", result.target),
        None => println!("{}: resolution completed", result.target),
    }
}

/// Human-readable commit log.
pub fn print_log(result: &LogResult) {
    if result.entries.is_empty() {
        println!("{}: no commits", result.name);
        return;
    }
    for e in &result.entries {
        println!("  {}  {}  ({}, {})", e.hash, e.subject, e.author, e.date);
    }
}
