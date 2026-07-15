# wtm: GitTower Replacement Plan

Living plan for turning wtm into a full replacement for the GitTower GUI. Update the status boxes as work lands. Last updated: 2026-07-10.

## Goal

Replace GitTower for everyday git work, driven from a TUI (and scriptable via CLI/MCP for AI agents). Priorities, in order:

1. Merge one branch into another.
2. "Update" a branch = merge the repo's default/main branch into a working branch.
3. A friendly conflict-resolution experience (diff compare, pick ours/theirs/both, or hand-edit the final result).

Non-goal: reimplementing all of git. Cover the everyday workflow well.

## Architecture (do not drift from this)

Three surfaces, one shared core. Keep all logic in the core; surfaces only call in.

- Core: `src/git.rs` (raw git exec + parsing), `src/ops.rs` (high-level ops, serde result types).
- TUI: `src/tui/app.rs` (state + `View` enum + key handling), `src/tui/ui.rs` (render). Long ops use the `View::Busy { rx, then }` background-thread pattern.
- CLI: `src/cli.rs`, `src/main.rs`, `--json` via `src/output.rs`.
- MCP: `src/mcp.rs` (stdio tools for agents).

Key existing patterns to mirror: `View::CherryPick`/`BranchCommits` (a multi-select + picker flow), `ops::cherry_pick`, serde result structs like `PullResult`. Note: `git::cherry_pick` auto-aborts on conflict; the new merge path deliberately does NOT (see below), and cherry-pick should later be changed to route conflicts into the resolver too.

## What already exists (before this effort)

Worktree create/list/remove; commit; stash push/list/pop/apply/drop; pull; push; fetch; branch create/delete/rename/list; log; cherry-pick; switch-branch. All wired across CLI/TUI/MCP.

## The gap

No merge, no update, and nothing handles conflicts — a conflicting op today either errors or (cherry-pick) auto-aborts. Conflict resolution is the biggest, highest-value piece.

## Work breakdown (task IDs match the session task list)

### Task 1 — Core: merge + update  ✅ DONE (2026-07-10)
`git.rs`: `MergeStatus { AlreadyUpToDate, Merged, Conflicted(Vec<String>) }`; `merge(dir, source_ref, no_ff)` (detects conflict via MERGE_HEAD + unmerged entries, does NOT auto-abort); `conflicted_files`; `is_merging`; `merge_abort`; `merge_continue` (`git commit --no-edit`).
`ops.rs`: `MergeOutcome { UpToDate, Clean{commit}, Conflicted{files} }`; `merge(ctx, target, source_branch, no_ff)`; `update(ctx, target)` (merges `default_branch`). Tests in the `ops.rs` `#[cfg(test)]` module (no lib target, so black-box `tests/integration.rs` can't call ops directly until a CLI command exists). New core items carry `#[allow(dead_code)]` until surfaces wire them — remove those allows in Tasks 3/4.

### Task 2 — Core: conflict parsing + resolution primitives  ✅ DONE (2026-07-10)
New `src/conflict.rs`: `ConflictSegment {Plain, Hunk{ours,theirs,base}}`, `ResolutionAction {KeepOurs,KeepTheirs,KeepBoth,KeepBothReversed,Manual}`, `parse`, `render` (KeepBoth = ours then theirs, separate lines), `marker_labels`. `git::checkout_conflict_side`. ops: `list_conflicts`, `read_conflict` (-> `ConflictFile`), `write_resolution`, `checkout_ours/theirs`, `complete_merge` (-> `CompleteMergeResult`, errors on unresolved), `abort_merge`. `mod conflict;` in main.rs. 159 tests pass, clippy clean.

<details><summary>original Task 2 spec</summary>
Parse a conflicted file into ordered segments: plain runs + conflict hunks (`<<<<<<< / |||||||  / ======= / >>>>>>>`, handle diff3 base). Per-hunk actions: KeepOurs, KeepTheirs, KeepBoth (ours then theirs on separate lines), KeepBothReversed, Manual(text). ops: `list_conflicts`, `read_conflict` (segments + ours/theirs labels), `write_resolution` (write + `git add`), whole-file `checkout_ours`/`checkout_theirs`, `complete_merge` (commit; error if unresolved remain), `abort_merge`. Tests for parsing (multi-hunk + diff3) and each action.
</details>

### Task 3 — TUI: merge/update entry points + conflict resolver view  ✅ DONE (2026-07-10)
`View::MergePick` + `View::ConflictResolver` (+ `ResolverFile`), `BusyThen::Resolve`. Branches tab `m` = merge into a picked worktree; Worktrees tab `u` = update (merge default branch in). Resolver: o/t keep ours/theirs, b/⇧B keep both / both-reversed, ⇧O/⇧T whole-file, w/Enter stage, c complete, x+y abort. Merge/update conflicts route in via list_conflicts after the Busy op. Manual per-hunk text edit deliberately omitted (follow-up). Cherry-pick/stash-pop routing needs core changes → see Task 6. 166 tests, clippy clean.

### Task 4 — CLI + MCP surfaces  ✅ DONE (2026-07-10)
CLI: `merge <src> --into <wt> [--no-ff|--continue|--abort]`, `update <wt>`, `conflicts <wt> [file]`, `resolve <wt> <file> --ours|--theirs|--both|--both-reversed`, all `--json`. MCP: `merge`, `update`, `list_conflicts`, `read_conflict`, `resolve_file`, `complete_merge`, `abort_merge`. (`--into` is required in all merge modes for consistency.) Integration tests drive a real conflict + resolve + continue.

### Task 5 — Verify end-to-end + docs  ✅ DONE (2026-07-10)
Full tree: build + 166 tests + clippy clean. Real CLI merge-conflict drive: merge→conflicted→`resolve --both` (MAIN then FEATURE on separate lines)→`merge --continue` committed. README updated (CLI/TUI/MCP + resolver keys + layout).

### Task 6 — Route cherry-pick + stash-pop conflicts into the resolver  ✅ DONE (2026-07-10)
`git.rs`: `cherry_pick` now returns `CherryPickStatus { Applied, Conflicted(files) }` (no more auto-abort on conflict; still cleans up genuine failures); added `is_cherry_picking`, `cherry_pick_continue` (`-c core.editor=true cherry-pick --continue`), `cherry_pick_abort`, `reset_hard`; `stash_pop` returns `StashPopStatus { Applied(out), Conflicted(files) }` (keeps the stash on conflict).
`ops.rs`: `CherryPickOutcome`/`StashPopOutcome` (status-tagged, with `Conflicted{files}`); a `ResolveKind { Merge, CherryPick, StashPop{index} }` op-kind; generalized `complete_resolution(ctx, target, kind, message)` and `abort_resolution(ctx, target, kind)` (dispatch: merge-continue/commit, cherry-pick-continue, or stash-drop; abort = merge/cherry-pick `--abort` or reset-hard keeping the stash); `detect_resolve_kind` (reads MERGE_HEAD/CHERRY_PICK_HEAD; `None` = stash pop, which has no marker). `complete_merge`/`abort_merge` removed in favor of the generalized pair.
Surfaces: TUI `BusyThen::Resolve` and `View::ConflictResolver` carry `kind`; cherry-pick and stash-pop flows route their `Conflicted` outcomes into the same resolver, whose complete/abort dispatch by kind (clean stash pop returns to the stash overlay). CLI `wtm merge --continue/--abort` auto-detects merge vs cherry-pick; `wtm cherry-pick`/`wtm stash pop` report a `conflicted` status with files (stash-pop resolution finishes with `wtm stash drop`). MCP `cherry_pick`/`stash_pop` tools return the conflicted outcome; `complete_merge`/`abort_merge` auto-detect merge/cherry-pick. 170 tests, build + clippy clean.

Design note: stash-pop resolution has no repo marker, so its completion needs the popped stash's index — carried explicitly in `ResolveKind::StashPop{index}` (the TUI knows it; the CLI/MCP finish a stash via `stash_drop` rather than unsafely guessing an index in `merge --continue`).

### Task 7 — Create dialog: remote branches, filter, and manual conflict edit  ✅ DONE (2026-07-13)
Follow-ups from the goal beyond the original merge/update/conflict scope.
`git.rs`: `remote_branches` lists fetched remote-tracking refs as `(short_name, remote_ref)`, skipping `origin/HEAD`.
TUI create dialog (`View::Create`): the checkout list is now `Vec<CheckoutCandidate>` (local not-checked-out branches plus remote-only branches, deduped by name); a remote candidate checks out into a local tracking branch off its `origin/…` ref via the existing `ops::create(from = Some(remote_ref))` path. The typed name doubles as a live case-insensitive **filter** over that list (`create_filtered`), so long branch lists are searchable while row 0 still names the new branch. Remote candidates render with a dim `(origin/…)` tag.
Conflict resolver **manual edit** (the last conflict-UX bullet): `e` opens `HunkEditor`, a minimal multi-line editor seeded from the chosen side (or both sides), saved with `Ctrl+S` as `ResolutionAction::Manual` (the previously-omitted per-hunk hand edit; `conflict::render` already honored `Manual`). `Esc` discards. Removed the `#[allow(dead_code)]` on `Manual`.
Tests: `remote_branches` parsing; create-dialog remote listing + type-to-filter; `HunkEditor` round-trip; resolver manual-edit save/discard end-to-end. 176 tests, build + clippy clean.

### Superseded original specs (Tasks 3-5)

#### Task 3 (original)
Merge picker (source branch → target worktree) from Branches tab / worktree list, run via `View::Busy`. "Update from main" action on a worktree. New `View::ConflictResolver`: ours/theirs diff compare per hunk, file+hunk navigation, keybinds keep ours/theirs/both/both-reversed/edit, per-file resolved indicator, complete-merge (commit) + abort. Route merge/update/cherry-pick/stash-pop conflicts into it; stop cherry-pick auto-aborting.

### Task 4 — CLI + MCP surfaces  🔄 IN PROGRESS (blocked-by: 2, parallel with 3)
CLI: `wtm merge <source> --into <worktree>`, `wtm update <worktree>`, `wtm conflicts <worktree>`, `wtm resolve <worktree> <file> --ours|--theirs|--both`, `wtm merge --continue|--abort`, all `--json`. MCP: tools `merge`, `update`, `list_conflicts`, `read_conflict`, `resolve_file`, `complete_merge`, `abort_merge` so an agent can run a full resolve loop. Add `tests/integration.rs` coverage for CLI json (this also lets black-box tests exercise the core).

### Task 5 — Verify end-to-end + docs  ⬜ (blocked-by: 3, 4)
Full `cargo test` + `clippy`; drive a real merge-with-conflict through the CLI end to end; update `README.md` (merge/update/conflict across all three surfaces) and `CLAUDE.md` if module roles shifted.

## Dependency graph

```
1 (done) ──▶ 2 ──▶ 3 ──┐
                 └▶ 4 ──┴─▶ 5
```

## Conflict-resolution UX decisions (from the goal)

- Diff-compare view with the ability to pick which side to keep per conflict.
- Must support "keep both" — put one change on a new line after the other (both orderings available).
- Must allow manually editing the final merged result.
- Same capability reachable from any conflicting op (merge, update, cherry-pick, stash pop).

## Notes / gotchas

- Crate has no lib target: core-only tests live in `src/*.rs` `#[cfg(test)]` modules; `tests/integration.rs` can only drive the built binary, so end-to-end conflict tests need the CLI (Task 4).
- No em dashes in code comments or docs (project style). Docblock on every function; inline comments only where non-obvious.
- Keep `cargo build` / `cargo test` / `cargo clippy` clean at every step.
