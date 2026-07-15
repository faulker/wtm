//! MCP server over stdio, exposing worktree operations as tools for AI agents.
//!
//! Register with e.g. `claude mcp add wtm -- wtm mcp` (run from inside the
//! repo). Tool results are the same JSON shapes as the `--json` CLI output.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::config::Config;
use crate::conflict;
use crate::ops::{self, Ctx};
use crate::output;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateRequest {
    #[schemars(description = "branch to check out in the new worktree (created if missing)")]
    branch: String,
    #[schemars(description = "base ref for a newly created branch (defaults to HEAD)")]
    from: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RemoveRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "discard uncommitted changes (default false)")]
    force: Option<bool>,
    #[schemars(description = "also delete the worktree's local branch (default false)")]
    delete_branch: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NameRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CommitRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "commit message")]
    message: String,
    #[schemars(description = "only stage these paths; defaults to staging every change")]
    paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StashPushRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "optional stash message")]
    message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StashIndexRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "stash entry index (defaults to 0, the most recent)")]
    index: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PullRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(
        description = "rebase local commits onto the upstream instead of fast-forwarding (default false)"
    )]
    rebase: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PushRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(
        description = "force-push, but only if the remote hasn't moved unexpectedly (default false)"
    )]
    force_with_lease: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SwitchRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "existing local branch to check out in the worktree")]
    branch: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchCreateRequest {
    #[schemars(description = "branch name")]
    name: String,
    #[schemars(description = "base ref for the new branch (defaults to HEAD)")]
    from: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchDeleteRequest {
    #[schemars(description = "branch name")]
    name: String,
    #[schemars(description = "delete even if unmerged, using -D (default false)")]
    force: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchRenameRequest {
    #[schemars(description = "current branch name")]
    old: String,
    #[schemars(description = "new branch name")]
    new: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "number of commits to show (default 20)")]
    count: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BranchLogRequest {
    #[schemars(description = "local branch name")]
    name: String,
    #[schemars(description = "number of commits to show (default 20)")]
    count: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MergeRequest {
    #[schemars(description = "local branch to merge in")]
    source: String,
    #[schemars(description = "worktree to merge into")]
    into: String,
    #[schemars(
        description = "force a merge commit even when a fast-forward would do (default false)"
    )]
    no_ff: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadConflictRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "conflicted file path, relative to the worktree root")]
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResolveFileRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "conflicted file path, relative to the worktree root")]
    path: String,
    #[schemars(
        description = "resolution to apply: \"ours\", \"theirs\", \"both\", \"both_reversed\", or \"manual\" (requires text)"
    )]
    action: String,
    #[schemars(
        description = "replacement text for the whole file; required when action is \"manual\""
    )]
    text: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompleteMergeRequest {
    #[schemars(description = "worktree name (branch name, or directory name when detached)")]
    name: String,
    #[schemars(description = "commit message (defaults to git's prepared merge message)")]
    message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CherryPickRequest {
    #[schemars(description = "worktree to apply the commits into")]
    into: String,
    #[schemars(
        description = "commit hashes to apply, ordered oldest-first (the order git applies them)"
    )]
    commits: Vec<String>,
    #[schemars(
        description = "load the changes into the working tree without committing (default false)"
    )]
    no_commit: Option<bool>,
}

/// MCP tool server bound to one repository. The `#[tool_handler]` impl routes
/// tool calls through `Self::tool_router()` generated by `#[tool_router]`.
struct WtmServer {
    repo_root: PathBuf,
}

impl WtmServer {
    /// Reloads config on every call so `.wtm.toml` edits take effect without
    /// restarting the server. Also re-checks the init gate per call, so the
    /// tools start working the moment `wtm init` runs.
    fn ctx(&self) -> Result<Ctx, ErrorData> {
        let config = Config::load(&self.repo_root).map_err(internal)?;
        let ctx = Ctx {
            repo_root: self.repo_root.clone(),
            config,
        };
        ctx.ensure_initialized().map_err(internal)?;
        Ok(ctx)
    }
}

/// Converts any operation error into an MCP internal error.
fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

/// Wraps a serializable result as a JSON text content block.
fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(value).map_err(internal)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[tool_router]
impl WtmServer {
    #[tool(
        description = "List all git worktrees with branch, path, dirty file count, and ahead/behind upstream counts"
    )]
    fn list_worktrees(&self) -> Result<CallToolResult, ErrorData> {
        json_result(&ops::list(&self.ctx()?).map_err(internal)?)
    }

    #[tool(
        description = "Create a worktree for a branch (creating the branch if needed), copy configured files, and run the repo's setup commands from .wtm.toml"
    )]
    fn create_worktree(
        &self,
        Parameters(req): Parameters<CreateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::create(
            &self.ctx()?,
            &req.branch,
            req.from.as_deref(),
            ops::RunMode::Capture,
            |_| {},
        )
        .map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Remove a worktree. Refuses when it has uncommitted changes unless force is true"
    )]
    fn remove_worktree(
        &self,
        Parameters(req): Parameters<RemoveRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let delete_branch = req.delete_branch.unwrap_or(false);
        let info = ops::remove(
            &self.ctx()?,
            &req.name,
            req.force.unwrap_or(false),
            delete_branch,
        )
        .map_err(internal)?;
        json_result(&output::remove_json(&info, delete_branch))
    }

    #[tool(description = "List the changed files (staged, unstaged, untracked) in a worktree")]
    fn worktree_status(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let (info, entries) = ops::status(&self.ctx()?, &req.name).map_err(internal)?;
        json_result(&output::status_json(&info, &entries))
    }

    #[tool(description = "Get the unified diff of a worktree's uncommitted changes")]
    fn worktree_diff(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let (info, diff) = ops::diff(&self.ctx()?, &req.name).map_err(internal)?;
        json_result(&output::diff_json(&info, &diff))
    }

    #[tool(
        description = "Stage and commit changes in a worktree. Stages every change by default, or only the given paths. Use this to save a worktree's in-progress work as a commit"
    )]
    fn commit_changes(
        &self,
        Parameters(req): Parameters<CommitRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::commit(&self.ctx()?, &req.name, &req.message, req.paths.as_deref())
            .map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Stash uncommitted changes (including untracked files) in a worktree, restoring a clean working tree. Use this to set aside in-progress work before switching tasks"
    )]
    fn stash_push(
        &self,
        Parameters(req): Parameters<StashPushRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result =
            ops::stash_push(&self.ctx()?, &req.name, req.message.as_deref()).map_err(internal)?;
        json_result(&result)
    }

    #[tool(description = "List a worktree's stash entries, newest first")]
    fn stash_list(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::stash_list(&self.ctx()?, &req.name).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Apply and drop a stash entry in a worktree (default: the most recent entry). Use this to restore previously stashed work. On success returns status \"applied\"; on conflicts returns status \"conflicted\" with the list of conflicted files, keeps the stash, and leaves the files to resolve via read_conflict/resolve_file, after which stash_drop finishes the pop"
    )]
    fn stash_pop(
        &self,
        Parameters(req): Parameters<StashIndexRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::stash_pop(&self.ctx()?, &req.name, req.index).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Apply a stash entry in a worktree without dropping it (default: the most recent entry). Use this to try out stashed changes while keeping the stash entry"
    )]
    fn stash_apply(
        &self,
        Parameters(req): Parameters<StashIndexRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::stash_apply(&self.ctx()?, &req.name, req.index).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Delete a stash entry in a worktree without applying it (default: the most recent entry). Use this to discard stashed changes that are no longer needed"
    )]
    fn stash_drop(
        &self,
        Parameters(req): Parameters<StashIndexRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::stash_drop(&self.ctx()?, &req.name, req.index).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Pull the latest changes for a worktree's branch from its upstream (fast-forward only by default; set rebase to rebase local commits onto the upstream instead). Errors if the branch has no upstream configured"
    )]
    fn pull_worktree(
        &self,
        Parameters(req): Parameters<PullRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result =
            ops::pull(&self.ctx()?, &req.name, req.rebase.unwrap_or(false)).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Push a worktree's branch to its remote, publishing to origin with -u automatically if it has no upstream yet. Set force_with_lease to force-push safely (only if the remote hasn't moved unexpectedly)"
    )]
    fn push_worktree(
        &self,
        Parameters(req): Parameters<PushRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::push(
            &self.ctx()?,
            &req.name,
            req.force_with_lease.unwrap_or(false),
        )
        .map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Fetch every remote for the repo and prune deleted remote-tracking branches"
    )]
    fn fetch_remotes(&self) -> Result<CallToolResult, ErrorData> {
        json_result(&ops::fetch(&self.ctx()?).map_err(internal)?)
    }

    #[tool(
        description = "Switch a worktree to check out a different existing local branch. Refuses if the branch is already checked out in another worktree"
    )]
    fn switch_branch(
        &self,
        Parameters(req): Parameters<SwitchRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::switch_branch(&self.ctx()?, &req.name, &req.branch).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "List local branches with upstream tracking, ahead/behind counts, last-commit info, and which worktree (if any) has each checked out"
    )]
    fn list_branches(&self) -> Result<CallToolResult, ErrorData> {
        json_result(&ops::branch_list(&self.ctx()?).map_err(internal)?)
    }

    #[tool(
        description = "Create a local branch without checking it out in a worktree. Use create_worktree instead when the branch needs a working directory right away"
    )]
    fn create_branch(
        &self,
        Parameters(req): Parameters<BranchCreateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result =
            ops::branch_create(&self.ctx()?, &req.name, req.from.as_deref()).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Delete a local branch. Refuses if it's checked out in a worktree (remove that worktree first) or unmerged, unless force is true"
    )]
    fn delete_branch(
        &self,
        Parameters(req): Parameters<BranchDeleteRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::branch_delete(&self.ctx()?, &req.name, req.force.unwrap_or(false))
            .map_err(internal)?;
        json_result(&result)
    }

    #[tool(description = "Rename a local branch")]
    fn rename_branch(
        &self,
        Parameters(req): Parameters<BranchRenameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::branch_rename(&self.ctx()?, &req.old, &req.new).map_err(internal)?;
        json_result(&result)
    }

    #[tool(description = "Show recent commits (newest first) for a worktree's branch")]
    fn worktree_log(
        &self,
        Parameters(req): Parameters<LogRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result =
            ops::log(&self.ctx()?, &req.name, req.count.unwrap_or(20)).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Show a local branch's commit history (newest first) without checking it out. Use this to find commit hashes to cherry-pick"
    )]
    fn branch_log(
        &self,
        Parameters(req): Parameters<BranchLogRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result =
            ops::branch_log(&self.ctx()?, &req.name, req.count.unwrap_or(20)).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Merge a local branch into the branch checked out in a worktree. On success returns status \"up_to_date\" or \"clean\" (with the new commit); on conflicts returns status \"conflicted\" with the list of conflicted files and leaves the worktree mid-merge for read_conflict/resolve_file/complete_merge"
    )]
    fn merge(
        &self,
        Parameters(req): Parameters<MergeRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::merge(
            &self.ctx()?,
            &req.into,
            &req.source,
            req.no_ff.unwrap_or(false),
        )
        .map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Merge the repository's default branch into a worktree, bringing it up to date with the mainline. Same result shape as merge"
    )]
    fn update(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::update(&self.ctx()?, &req.name).map_err(internal)?;
        json_result(&result)
    }

    #[tool(description = "List the conflicted (unmerged) files in a worktree mid-merge")]
    fn list_conflicts(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let files = ops::list_conflicts(&self.ctx()?, &req.name).map_err(internal)?;
        json_result(&output::conflicts_json(&req.name, &files))
    }

    #[tool(
        description = "Read and parse a conflicted file in a worktree mid-merge into its plain-text and conflict-hunk segments, so a resolution can be worked out for each hunk"
    )]
    fn read_conflict(
        &self,
        Parameters(req): Parameters<ReadConflictRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::read_conflict(&self.ctx()?, &req.name, &req.path).map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Resolve a conflicted file in a worktree mid-merge and stage it. \"ours\"/\"theirs\" keep one side of every hunk whole; \"both\"/\"both_reversed\" keep both sides of every hunk concatenated in that order; \"manual\" writes the given text as the file's full resolved contents"
    )]
    fn resolve_file(
        &self,
        Parameters(req): Parameters<ResolveFileRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let ctx = self.ctx()?;
        match req.action.as_str() {
            "ours" => ops::checkout_ours(&ctx, &req.name, &req.path).map_err(internal)?,
            "theirs" => ops::checkout_theirs(&ctx, &req.name, &req.path).map_err(internal)?,
            "both" | "both_reversed" => {
                let file = ops::read_conflict(&ctx, &req.name, &req.path).map_err(internal)?;
                let action = if req.action == "both" {
                    conflict::ResolutionAction::KeepBoth
                } else {
                    conflict::ResolutionAction::KeepBothReversed
                };
                let hunks = file
                    .segments
                    .iter()
                    .filter(|s| matches!(s, conflict::ConflictSegment::Hunk { .. }))
                    .count();
                let text = conflict::render(&file.segments, &vec![action; hunks]);
                ops::write_resolution(&ctx, &req.name, &req.path, &text).map_err(internal)?;
            }
            "manual" => {
                let text = req.text.ok_or_else(|| {
                    ErrorData::invalid_params("manual resolution requires text", None)
                })?;
                ops::write_resolution(&ctx, &req.name, &req.path, &text).map_err(internal)?;
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!(
                        "unknown action '{other}'; expected ours, theirs, both, both_reversed, or manual"
                    ),
                    None,
                ));
            }
        }
        json_result(&output::resolve_json(&req.name, &req.path, &req.action))
    }

    #[tool(
        description = "Finish an in-progress merge or cherry-pick in a worktree once every conflict has been resolved and staged. Auto-detects which is in progress. Errors if conflicts remain or neither is in progress. To finish a resolved stash pop, drop the stash with stash_drop instead"
    )]
    fn complete_merge(
        &self,
        Parameters(req): Parameters<CompleteMergeRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let ctx = self.ctx()?;
        let kind = ops::detect_resolve_kind(&ctx, &req.name)
            .map_err(internal)?
            .ok_or_else(|| {
                internal(anyhow::anyhow!(
                    "no merge or cherry-pick in progress in '{}'",
                    req.name
                ))
            })?;
        let result = ops::complete_resolution(&ctx, &req.name, kind, req.message.as_deref())
            .map_err(internal)?;
        json_result(&result)
    }

    #[tool(
        description = "Abandon an in-progress merge or cherry-pick in a worktree, restoring its pre-operation state. Auto-detects which is in progress"
    )]
    fn abort_merge(
        &self,
        Parameters(req): Parameters<NameRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let ctx = self.ctx()?;
        let kind = ops::detect_resolve_kind(&ctx, &req.name)
            .map_err(internal)?
            .ok_or_else(|| {
                internal(anyhow::anyhow!(
                    "no merge or cherry-pick in progress in '{}'",
                    req.name
                ))
            })?;
        ops::abort_resolution(&ctx, &req.name, kind).map_err(internal)?;
        json_result(&serde_json::json!({ "target": req.name, "aborted": true }))
    }

    #[tool(
        description = "Cherry-pick one or more commits (from any branch) into a worktree. Commits are applied oldest-first. With no_commit the changes are staged into the working tree without committing, so they can be reviewed or edited first; otherwise each commit is recorded with its original message. On success returns status \"applied\"; on conflicts returns status \"conflicted\" with the list of conflicted files and leaves the worktree mid-cherry-pick for read_conflict/resolve_file/complete_merge (which continues the cherry-pick) or abort_merge"
    )]
    fn cherry_pick(
        &self,
        Parameters(req): Parameters<CherryPickRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = ops::cherry_pick(
            &self.ctx()?,
            &req.into,
            &req.commits,
            req.no_commit.unwrap_or(false),
        )
        .map_err(internal)?;
        json_result(&result)
    }
}

#[tool_handler]
impl ServerHandler for WtmServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Manage git worktrees for this repository: create (with automated setup from \
                 .wtm.toml), list, inspect status/diffs, and remove worktrees. Also supports \
                 per-worktree commits, stashes, pulls, and pushes, plus repo-wide fetch and \
                 branch management (list/create/delete/rename), commit history, and merging \
                 (merge/update, then list_conflicts/read_conflict/resolve_file/complete_merge \
                 or abort_merge when a merge stops on conflicts).",
            );
        info.server_info.name = env!("CARGO_PKG_NAME").into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info
    }
}

/// Serves MCP over stdio until the client disconnects.
pub fn serve(ctx: Ctx) -> Result<()> {
    let server = WtmServer {
        repo_root: ctx.repo_root,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
