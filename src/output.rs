//! Rendering of command results as human-readable text or JSON.

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::git::StatusEntry;
use crate::ops::{CreateResult, WorktreeInfo};

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
