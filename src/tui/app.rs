//! TUI application state and key handling.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::config_editor::{self, ConfigEditor, EditorOutcome};
use super::help::HelpTab;
use super::highlight;
use super::setup::{self, SetupWizard, WizardOutcome};
use super::theme;
use crate::config::{CommandMode, OpenCommand, WorktreesLayout};
use crate::conflict::{self, ConflictSegment, ResolutionAction};
use crate::git::{self, GraphLine, LogEntry, StashEntry, StatusEntry};
use crate::ops::{self, BranchListItem, ConflictFile, Ctx, SetupControl, WorktreeInfo};
use crate::platform;
use crate::settings::ConfigDraft;
use crate::update::{self, CheckOutcome, Release};

/// A single-line text field with a movable insertion cursor. `cursor` is a
/// character index in `0..=value.chars().count()`, so `←/→`, Home/End, and
/// mid-string insert/delete all work instead of edit-at-the-end only.
#[derive(Default, Clone)]
pub struct TextInput {
    pub value: String,
    pub cursor: usize,
}

/// Byte offset of character index `idx` in `s`, or `s.len()` when past the end.
/// Keeps single-line edits on UTF-8 char boundaries. Shared by `TextInput` and
/// the multi-line `TextArea` so the boundary math lives in one place.
fn char_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Inserts `c` at character index `*cursor` in `s`, advancing the cursor.
fn line_insert(s: &mut String, cursor: &mut usize, c: char) {
    let b = char_byte(s, *cursor);
    s.insert(b, c);
    *cursor += 1;
}

/// Deletes the character before the cursor (Backspace), if any.
fn line_backspace(s: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        let start = char_byte(s, *cursor - 1);
        let end = char_byte(s, *cursor);
        s.replace_range(start..end, "");
        *cursor -= 1;
    }
}

/// Deletes the character at the cursor (Delete), if any.
fn line_delete(s: &mut String, cursor: usize) {
    if cursor < s.chars().count() {
        let start = char_byte(s, cursor);
        let end = char_byte(s, cursor + 1);
        s.replace_range(start..end, "");
    }
}

impl TextInput {
    fn len(&self) -> usize {
        self.value.chars().count()
    }

    fn insert(&mut self, c: char) {
        line_insert(&mut self.value, &mut self.cursor, c);
    }

    fn backspace(&mut self) {
        line_backspace(&mut self.value, &mut self.cursor);
    }

    fn delete(&mut self) {
        line_delete(&mut self.value, self.cursor);
    }

    /// Applies an editing key, returning true when it was consumed as text
    /// editing (so callers can treat other keys as their own actions).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn trimmed(&self) -> String {
        self.value.trim().to_string()
    }

    /// A prefilled input with the cursor at the end, for edit-in-place prompts
    /// like rename.
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }
}

/// How a worktree create finished. Drives the pinned banner in
/// `View::Creating` instead of being appended to the scrolling log, so a long
/// `npm install` can't push the "ready" signal off screen.
pub struct CreateOutcome {
    /// True only when the worktree exists *and* every setup step succeeded.
    pub ok: bool,
    /// Worktree path, empty when the create itself failed.
    pub path: String,
    /// Error text when the create failed outright.
    pub detail: Option<String>,
}

/// Message from the background create thread.
pub enum CreateMsg {
    Progress(String),
    Done(Result<crate::ops::CreateResult, String>),
}

/// A background operation's result channel, wrapping the `mpsc::Receiver`
/// shape shared by `Diff`/`CommitDiff`'s diff loads, `Creating`'s setup
/// progress, and `Busy`'s op result.
pub struct Task<T> {
    rx: Receiver<T>,
}

impl<T> Task<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx }
    }

    /// Drains every value currently queued and returns only the most recent
    /// one, so a burst of results (e.g. fast navigation re-triggering a diff
    /// load) never applies a stale intermediate value. Used where only the
    /// latest message matters.
    fn poll_latest(&self) -> Option<T> {
        let mut latest = None;
        while let Ok(msg) = self.rx.try_recv() {
            latest = Some(msg);
        }
        latest
    }

    /// A single non-blocking receive. Used where every queued message must
    /// be processed in order (e.g. `Creating`'s progress stream), unlike
    /// `poll_latest`, which discards all but the newest.
    fn try_recv(&self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

/// How often the diff view recomputes itself to pick up outside edits.
const DIFF_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

/// How often the worktree/branch lists reload themselves so work done outside
/// the app (an agent committing, a teammate's branch landing) shows up without
/// pressing `r`. Only fires while the plain list is on screen; see
/// `auto_refresh`.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// How long a status/error message stays on screen before auto-clearing.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(4);

/// After q/Esc navigates "back", ignore a quit-bound q/Esc for this long so a
/// key buffered during a slow frame cannot quit once the pop finally lands.
const QUIT_IGNORE_AFTER_BACK: Duration = Duration::from_millis(400);

/// How commit history is drawn in the log and branch-commit views. `Tree` runs
/// the log through `git log --graph` so branch and merge topology is visible;
/// `Flat` is a plain newest-first list. Toggled with `t`, and remembered on the
/// `App` so the choice sticks across views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    Tree,
    Flat,
}

impl LogMode {
    fn toggled(self) -> LogMode {
        match self {
            LogMode::Tree => LogMode::Flat,
            LogMode::Flat => LogMode::Tree,
        }
    }

    /// Label for the header/help, naming the current mode.
    pub fn label(self) -> &'static str {
        match self {
            LogMode::Tree => "tree",
            LogMode::Flat => "flat",
        }
    }
}

/// Commit-history rows for a branch or worktree, honouring `log_mode`. Free
/// function so background threads can load the three-panel commits list without
/// borrowing `App`.
fn commit_log_lines(
    ctx: &Ctx,
    name: &str,
    branch: &str,
    from_branch: bool,
    mode: LogMode,
) -> Result<Vec<GraphLine>, String> {
    match (from_branch, mode) {
        (true, LogMode::Tree) => {
            ops::branch_log_graph(ctx, branch, 200).map_err(|e| format!("{e:#}"))
        }
        (true, LogMode::Flat) => ops::branch_log(ctx, branch, 200)
            .map(|r| flat_lines(r.entries))
            .map_err(|e| format!("{e:#}")),
        (false, LogMode::Tree) => ops::log_graph(ctx, name, 100).map_err(|e| format!("{e:#}")),
        (false, LogMode::Flat) => ops::log(ctx, name, 100)
            .map(|r| flat_lines(r.entries))
            .map_err(|e| format!("{e:#}")),
    }
}

/// Identity of a three-panel commits-panel load, used to drop stale background
/// results when the selection, branch, or log mode has since changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitsLoadKey {
    name: String,
    branch: String,
    from_branch: bool,
    log_mode: LogMode,
}

type CommitsPending = Option<(CommitsLoadKey, Task<Result<Vec<GraphLine>, String>>)>;
type StatusRefreshPending = Option<(u64, String, bool, Task<Result<Vec<StatusEntry>, String>>)>;
type StashPending = Option<(String, Task<Result<Vec<StashEntry>, String>>)>;

/// Index of the first row holding a commit, skipping any leading art-only rows.
/// 0 when there are none (an empty list has nothing to select anyway).
fn first_commit_row(lines: &[GraphLine]) -> usize {
    lines.iter().position(|l| l.entry.is_some()).unwrap_or(0)
}

/// The next row at or after `from` (searching in `dir`'s direction) that holds a
/// commit, so the cursor steps between commits rather than stopping on the
/// connector rows the graph draws between them. `None` when there is no further
/// commit that way, leaving the cursor put.
fn seek_commit_row(lines: &[GraphLine], from: usize, forward: bool) -> Option<usize> {
    let mut i = from;
    loop {
        i = if forward {
            i.checked_add(1).filter(|i| *i < lines.len())?
        } else {
            i.checked_sub(1)?
        };
        if lines[i].entry.is_some() {
            return Some(i);
        }
    }
}

/// Presents flat log entries as graph lines carrying no art, so the tree and
/// flat views share a single row type and rendering path.
fn flat_lines(entries: Vec<LogEntry>) -> Vec<GraphLine> {
    entries
        .into_iter()
        .map(|e| GraphLine {
            graph: String::new(),
            entry: Some(e),
        })
        .collect()
}

/// A branch offered for checkout in the new-worktree dialog. Local branches
/// carry `remote: None` and are checked out directly. Remote-only branches (a
/// teammate's branch that has no local copy yet) carry their remote ref (e.g.
/// `origin/feature`), so selecting one creates a local tracking branch from it.
#[derive(Debug, Clone)]
pub struct CheckoutCandidate {
    /// Local branch name to check out or create.
    pub branch: String,
    /// Remote ref to base a new tracking branch on; `None` for a local branch.
    pub remote: Option<String>,
}

/// Indices into `branches` whose name matches `filter` (case-insensitive
/// substring); an empty filter matches everything. Used by the create dialog and
/// the switch picker, in each case by both the key handling and the renderer, so
/// that the two stay in lockstep.
pub fn filtered_candidates(branches: &[CheckoutCandidate], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    branches
        .iter()
        .enumerate()
        .filter(|(_, c)| needle.is_empty() || c.branch.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// One row of the upstream picker: either "stop tracking" or a remote ref.
/// Shared by the key handling and the renderer so the cursor and what's drawn
/// cannot disagree about which row means what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamRow {
    /// Clears the branch's upstream. Only present when it has one to clear.
    Unset,
    /// Track this remote-tracking ref (an index into the picker's candidates).
    Candidate(usize),
}

/// The picker's visible rows for `candidates` narrowed by `filter`, with the
/// "stop tracking" row first when `has_upstream`. An empty filter matches
/// everything.
pub fn upstream_rows(candidates: &[String], filter: &str, has_upstream: bool) -> Vec<UpstreamRow> {
    let needle = filter.trim().to_lowercase();
    let mut rows: Vec<UpstreamRow> = has_upstream
        .then_some(UpstreamRow::Unset)
        .into_iter()
        .collect();
    rows.extend(
        candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| needle.is_empty() || c.to_lowercase().contains(&needle))
            .map(|(i, _)| UpstreamRow::Candidate(i)),
    );
    rows
}

/// One row of the Branches tab's table: a group heading or a branch.
///
/// Local and remote-only branches are drawn as two labelled groups, so the
/// table has rows the cursor never lands on. `branch_display_rows` is the one
/// place that mapping is computed; the renderer and the click handler both go
/// through it so a click can't select a heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchRow {
    /// A group heading; carries the group's label.
    Header(&'static str),
    /// A branch, by its index into `App::branches`.
    Branch(usize),
}

/// The Branches tab's display rows for `branches`: the local branches under a
/// "local" heading, then the remote-only ones under a "remote" heading. A group
/// with no branches contributes no heading, so a repo with no remotes looks the
/// same as it always did.
///
/// `branches` is expected to arrive local-first (see `ops::branch_list`), but
/// this groups by each row's own `remote` flag rather than trusting the order,
/// so a stale or re-sorted list still renders under the right heading.
pub fn branch_display_rows(branches: &[BranchListItem]) -> Vec<BranchRow> {
    let mut rows = Vec::with_capacity(branches.len() + 2);
    for (label, remote) in [("LOCAL BRANCHES", false), ("REMOTE BRANCHES", true)] {
        let group: Vec<usize> = branches
            .iter()
            .enumerate()
            .filter(|(_, b)| b.remote.is_some() == remote)
            .map(|(i, _)| i)
            .collect();
        if group.is_empty() {
            continue;
        }
        rows.push(BranchRow::Header(label));
        rows.extend(group.into_iter().map(BranchRow::Branch));
    }
    rows
}

/// The display row showing branch `index`, for placing the table's selection.
pub fn branch_row_of(rows: &[BranchRow], index: usize) -> usize {
    rows.iter()
        .position(|r| *r == BranchRow::Branch(index))
        .unwrap_or(0)
}

/// A modal overlay drawn on top of the active screen: a confirmation, a
/// single-line prompt, or the manual hunk editor. Only one is ever open at a
/// time (`App::modal`); the screen underneath stays put and reappears when the
/// modal is dismissed. This absorbs the former per-screen confirm/prompt fields
/// (`Diff`'s revert/delete/ignore, `ConflictResolver`'s abort/edit) and the
/// standalone `BranchMode`/`StashMode` sub-modes.
pub enum Modal {
    /// A choice between one or more options. `body` carries the already-
    /// interpolated prompt text; `options` are the selectable rows. Resolved by
    /// `on_modal_key` into `ModalResult::Confirmed(index)` or `Cancelled`.
    Confirm {
        title: String,
        body: Vec<Line<'static>>,
        options: Vec<ConfirmOption>,
        /// Currently highlighted option (an index into `options`).
        selected: usize,
        /// What the calling screen wants done with the result.
        action: ModalAction,
    },
    /// A single-line text prompt, resolved into `Submitted(text)` or `Cancelled`.
    Prompt {
        title: String,
        input: TextInput,
        hint: String,
        action: ModalAction,
    },
    /// The manual conflict-hunk editor. Unlike the others it edits in place and
    /// saves back into the `ConflictResolver` screen underneath on Ctrl+S,
    /// rather than producing a `ModalResult`.
    HunkEditor(TextArea),
    /// The whole-file editor: the conflicted file exactly as it sits on disk,
    /// conflict markers included, for the cases the per-hunk choices can't
    /// express (interleaving both sides, fixing up an import list, deleting a
    /// stray line). Ctrl+S writes it straight back to disk and re-reads it.
    FileEditor { path: String, editor: TextArea },
}

/// One selectable row of a `Modal::Confirm`.
pub struct ConfirmOption {
    /// Optional single-key shortcut that confirms this option directly (in
    /// addition to navigating to it and pressing Enter). `None` for plain radio
    /// options with no mnemonic.
    pub key: Option<char>,
    pub label: String,
    /// Marks an option that discards work. Its `shortcut` is the Shift-variant
    /// of `key`, so a destructive action can't share a lowercase key with a
    /// screen-global binding (see `ConfirmOption::shortcut`).
    pub destructive: bool,
    /// False for an option shown but not choosable (e.g. "open" when the target
    /// is not a worktree). Navigation skips disabled options and they render
    /// dimmed.
    pub enabled: bool,
}

impl ConfirmOption {
    /// A plain, enabled option with no shortcut.
    fn new(label: impl Into<String>) -> Self {
        Self {
            key: None,
            label: label.into(),
            destructive: false,
            enabled: true,
        }
    }

    fn key(mut self, key: char) -> Self {
        self.key = Some(key);
        self
    }

    fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The effective single-key shortcut. A destructive option is always the
    /// Shift-variant of its key, so a bare lowercase used elsewhere on the
    /// screen (e.g. `f` = fetch) can never trigger a force/delete by accident.
    /// This is enforced here rather than at each call site so a new destructive
    /// option can't reintroduce the collision by forgetting to uppercase.
    pub fn shortcut(&self) -> Option<char> {
        self.key.map(|c| {
            if self.destructive {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
    }
}

/// The outcome of a resolved `Modal`, handed to `dispatch_modal` so the calling
/// screen can carry out its effect.
pub enum ModalResult {
    /// A `Confirm` option at this index was chosen.
    Confirmed(usize),
    /// A `Prompt`'s text was submitted (already trimmed).
    Submitted(String),
    /// The modal was dismissed without choosing (Esc/q/n).
    Cancelled,
}

/// What a modal's calling screen wants done once the modal resolves. The modal
/// carries only presentation; the effect lives here so `on_modal_key` stays
/// generic and the per-action logic stays debuggable (a plain enum, not a
/// boxed closure).
pub enum ModalAction {
    /// Revert the file under the Diff cursor (discard its changes).
    RevertFile,
    /// Delete the file under the Diff cursor from the worktree.
    DeleteFile,
    /// Discard every uncommitted change in the Changes tab's worktree.
    DiscardAllChanges,
    /// Add to `.gitignore`: option 0 ignores the exact `file`, option 1 the
    /// derived `pattern`.
    IgnorePath { file: String, pattern: String },
    /// The branch a create would start from (`base`, or `branch` itself when
    /// checking out an existing local branch) is behind its upstream: option 0
    /// pulls it first, 1 creates without pulling.
    ConfirmPullBase {
        branch: String,
        base: Option<String>,
        target: String,
    },
    /// The new-worktree target directory already exists: option 0 opens it (a
    /// worktree), 1 replaces it, 2 cancels.
    ConfirmExisting {
        branch: String,
        base: Option<String>,
        path: String,
        existing_name: Option<String>,
    },
    /// Replacing the directory would discard work: option 0 force-deletes and
    /// recreates, 1 cancels.
    ConfirmReplaceChanges {
        branch: String,
        base: Option<String>,
        path: String,
    },
    /// Remove a worktree: option index is `delete_branch` (0 = folder only,
    /// 1 = folder and branch, when it has a branch). `dirty` is the cached
    /// uncommitted count for the fallback dirtiness check.
    DeleteWorktree {
        name: String,
        dirty: usize,
        branch: Option<String>,
    },
    /// The worktree being removed is dirty: option 0 stashes then removes,
    /// 1 discards and removes, 2 cancels.
    DeleteWorktreeDirty {
        name: String,
        branch: Option<String>,
        delete_branch: bool,
    },
    /// Update a dirty worktree: option 0 stash+update+reapply, 1 update as-is,
    /// 2 cancel.
    UpdateStash { name: String },
    /// A branch could not be safely deleted after its folder was removed: option
    /// 0 force-deletes; cancelling keeps it.
    ForceBranch { branch: String },
    /// A fast-forward pull was refused: option 0 retries with a rebase.
    PullRebase { name: String },
    /// Create a new branch from HEAD named by the submitted text.
    BranchCreate,
    /// Rename `old` to the submitted text.
    BranchRename { old: String },
    /// Delete a branch: option 0 = normal delete, 1 = force delete.
    BranchDelete { name: String },
    /// Stash the worktree's changes with the submitted (optional) message.
    StashPush { name: String },
    /// Drop the stash entry `index` on `name` (option 0 confirms).
    StashDrop { name: String, index: Option<u32> },
    /// Abort the in-progress operation in the conflict resolver (option 0).
    ResolverAbort,
    /// A newer wtm is published: option 0 installs it and restarts, 1 postpones
    /// until the next launch.
    UpdateApp(Box<Release>),
}

/// Which screen/overlay is active.
pub enum View {
    List,
    /// New-worktree dialog. Row 0 creates a new branch (named in `name`) off
    /// `base`; the rows below check out an existing branch. The `name` field
    /// doubles as a live filter over the checkout list, so typing narrows the
    /// existing branches while also naming the would-be new branch.
    Create {
        /// Name of the new branch (row 0) and the live filter over `branches`.
        name: TextInput,
        /// Checkout options: local branches not checked out anywhere, plus
        /// remote-only branches (someone else's work) that have no local branch.
        branches: Vec<CheckoutCandidate>,
        /// Every local branch, for choosing a base to branch off of.
        all_branches: Vec<String>,
        /// Base ref a new branch is created from (defaults to the main branch).
        base: String,
        /// 0 = new branch; 1..=filtered.len() = check out the Nth *filtered*
        /// candidate (see `filtered_candidates`), not `branches` directly.
        selected: usize,
        /// True when the `[ Base: … ⌄ ]` button is focused (via Tab from the
        /// new-branch row), so Enter/Space opens the base picker instead of
        /// creating. Only meaningful while `selected == 0`.
        base_focus: bool,
        /// Some(idx) while picking the base branch: index into `all_branches`.
        base_pick: Option<usize>,
    },
    /// Progress of an in-flight create running on a background thread.
    Creating {
        branch: String,
        lines: Vec<String>,
        rx: Task<CreateMsg>,
        done: bool,
        /// Set once the create thread reports back. Rendered as a banner
        /// pinned to the bottom of the popup, so it survives the log's
        /// tail-trim no matter how much output the setup commands produced.
        outcome: Option<CreateOutcome>,
        /// Handle for sending input to / killing the running setup command.
        control: SetupControl,
        /// Pending line of user input for a prompting setup command.
        input: String,
        /// True after one Ctrl+C; the next one kills the setup.
        kill_armed: bool,
    },
    /// Prompt for a one-off command to run in a worktree's directory, shown by
    /// the `o` key when no `open_command` is configured.
    RunCommand {
        name: String,
        path: String,
        input: TextInput,
    },
    /// Picker for which configured `open_command` to run against the selected
    /// worktree. Shown whenever at least one command is configured, so the
    /// expanded command is always previewed before it runs.
    OpenCommand {
        /// Worktree the command will run against (for the title and expansion).
        name: String,
        /// Absolute path of that worktree.
        path: String,
        /// Branch name, or empty when detached.
        branch: String,
        /// Expanded `{status}` value for the templates.
        status: String,
        /// Configured commands (templates before `{…}` expansion, each with the
        /// run mode that decides whether the TUI stays open).
        commands: Vec<OpenCommand>,
        /// Cursor into `commands`.
        selected: usize,
    },
    /// Prompt for a worktree's new name, shown by the `R` key on the Worktrees
    /// tab. Submitting renames the branch and moves the directory to match.
    RenameWorktree {
        /// Current name of the worktree being renamed.
        name: String,
        /// New name, prefilled with the current one.
        input: TextInput,
    },
    /// First-run setup wizard, shown until `.wtm.toml` exists.
    Setup(Box<SetupWizard>),
    /// Commit flow: pick which changed files to include (all by default) and
    /// type a message. Focus cycles between the file list, the subject line,
    /// and the optional body.
    Commit {
        name: String,
        files: Vec<StatusEntry>,
        /// Whether each file is staged for this commit, parallel with `files`.
        marked: Vec<bool>,
        /// Cursor into `files` while the file list has focus.
        cursor: usize,
        input: TextInput,
        /// Optional commit body, below the subject line.
        body: TextArea,
        focus: CommitFocus,
    },
    /// Picker for switching the selected worktree onto a different branch: any
    /// local branch not checked out elsewhere, plus remote-only branches.
    Switch {
        /// Worktree being switched.
        name: String,
        /// Branches available to switch to (not checked out in any worktree).
        /// Remote-only ones carry their remote ref and become local tracking
        /// branches when picked.
        branches: Vec<CheckoutCandidate>,
        /// Live type-to-filter text; narrows `branches` by case-insensitive
        /// substring match. With no match, Enter tries the text as a branch name.
        filter: TextInput,
        /// Cursor into the FILTERED branch list, not `branches` directly.
        selected: usize,
    },
    /// Commit log for one worktree with a movable cursor. Rows are graph lines:
    /// in `LogMode::Tree` some carry only art (no commit), in `LogMode::Flat`
    /// every row is a commit with no art. Enter opens the commit browser
    /// (`CommitDiff`) for the commit under the cursor.
    Log {
        name: String,
        lines: Vec<GraphLine>,
        /// Cursor into `lines`; the cursor skips art-only rows.
        selected: usize,
    },
    /// Read-only browser for the files changed by one commit: the changed files
    /// on the left (tree or flat, shared with the changes view via `file_tree`)
    /// and the selected file's diff on the right. Diffs load off the UI thread
    /// exactly like `Diff`. Reached with Enter from `Log`.
    CommitDiff {
        /// Worktree the commit is viewed from (addressed by name in ops).
        name: String,
        /// Full commit hash being browsed.
        hash: String,
        /// Short hash + subject, for the panel title.
        label: String,
        files: Vec<StatusEntry>,
        rows: Vec<DiffRow>,
        selected: usize,
        content: String,
        content_path: Option<String>,
        load_gen: u64,
        pending: Option<Task<(u64, String, String)>>,
        loading_new: bool,
        scroll: u16,
        /// Horizontal offset into the diff pane (Shift+←/→ or H/L).
        h_scroll: u16,
    },
    /// Read-only browser for the files inside a stash entry. Same layout and
    /// keys as [`View::CommitDiff`]; reached with Enter from the Stash tab.
    StashDiff {
        /// Worktree supplying the git directory (stashes are repo-global).
        name: String,
        /// Stash index (`stash@{index}`).
        index: u32,
        /// Title shown in the panel (`stash@{n}: subject`).
        label: String,
        files: Vec<StatusEntry>,
        rows: Vec<DiffRow>,
        selected: usize,
        content: String,
        content_path: Option<String>,
        load_gen: u64,
        pending: Option<Task<(u64, String, String)>>,
        loading_new: bool,
        scroll: u16,
        h_scroll: u16,
    },
    /// Commit history of a branch on the Branches tab, with multi-select for
    /// cherry-picking. `marked` is parallel with `lines`; art-only rows are
    /// never marked and the cursor skips over them. Enter opens the worktree
    /// picker (`CherryPick`) for the marked commits (or the one under the
    /// cursor when none are marked).
    BranchCommits {
        branch: String,
        lines: Vec<GraphLine>,
        marked: Vec<bool>,
        selected: usize,
    },
    /// Cherry-pick flow: choose which worktree to apply the picked commits into,
    /// then whether to commit them or just load the changes. Reached from
    /// `BranchCommits`.
    CherryPick {
        /// Branch the commits came from (for labelling).
        source_branch: String,
        /// Commit hashes to apply, ordered oldest-first (git's apply order).
        commits: Vec<String>,
        /// Short subjects of `commits`, oldest-first, for display.
        summaries: Vec<String>,
        /// Worktrees the commits can be applied into.
        targets: Vec<CherryTarget>,
        /// Cursor into `targets`.
        selected: usize,
        /// None while picking the target; Some(0) = "commit", Some(1) = "load
        /// changes only" while the mode prompt is open.
        mode: Option<usize>,
    },
    /// Merge picker: choose which worktree (the target) to merge the branch
    /// selected on the Branches tab into. Reached from the Branches tab; runs
    /// the merge in the background and routes conflicts into the resolver.
    MergePick {
        /// Branch being merged in (the source).
        source_branch: String,
        /// Worktrees the branch can be merged into.
        targets: Vec<CherryTarget>,
        /// Cursor into `targets`.
        selected: usize,
    },
    /// Picks which worktree to rebase onto a chosen branch. The mirror image of
    /// `MergePick`: there the branch is the source, here it is the destination.
    RebasePick {
        /// Branch the worktree's commits will be replayed onto.
        onto_branch: String,
        /// Worktrees that can be rebased.
        targets: Vec<CherryTarget>,
        /// Cursor into `targets`.
        selected: usize,
    },
    /// Move-changes picker: choose which worktree to move the selected
    /// worktree's uncommitted changes into. Reached from the Worktrees tab's
    /// `m` key; runs `ops::move_changes` in the background.
    MoveChanges {
        /// Worktree the changes are moved out of.
        from: String,
        /// Worktrees the changes can be moved into (every other worktree).
        targets: Vec<CherryTarget>,
        /// Cursor into `targets`.
        selected: usize,
    },
    /// Destination picker for a stash apply/pop: stashes are repo-global, not
    /// tied to the worktree the Stash tab happens to be showing, so applying
    /// one asks which worktree to apply it into. Defaults the cursor to the
    /// worktree the tab was opened from. Reached from the Stash tab's `a`/`p`
    /// keys.
    StashTarget {
        /// True for pop (apply then drop); false for apply (keeps the entry).
        pop: bool,
        /// Stash entry being applied/popped (`None` means the most recent).
        index: Option<u32>,
        /// Short label for the entry, e.g. "stash@{0}", for the picker title.
        label: String,
        /// Worktrees the stash can be applied into.
        targets: Vec<CherryTarget>,
        /// Cursor into `targets`.
        selected: usize,
    },
    /// Picker for the remote branch a local branch tracks, opened by `u` on the
    /// Branches tab. Row 0 removes tracking (only offered when there is some);
    /// the rows below are the repo's remote-tracking refs, narrowed live by
    /// `filter` since a busy repo can have dozens.
    UpstreamPick {
        /// Local branch whose upstream is being changed.
        branch: String,
        /// What it tracks now, shown so the change reads as a change.
        current: Option<String>,
        /// Every remote-tracking ref (`origin/main`, …), newest commit first.
        candidates: Vec<String>,
        /// Live type-to-filter text over `candidates`.
        filter: TextInput,
        /// Cursor over the visible rows (see `upstream_rows`), not over
        /// `candidates` directly.
        selected: usize,
    },
    /// Friendly conflict resolver for a worktree left mid-merge. Lists the
    /// conflicted files, and for the selected file shows each hunk's OURS vs
    /// THEIRS sides so a resolution can be picked per hunk (or the whole file
    /// taken from one side), then staged. Reached when a merge/update conflicts.
    ConflictResolver {
        /// Worktree being resolved (addressed by name in ops).
        target: String,
        /// What is being merged in, for the header (e.g. the source branch).
        source_label: String,
        /// The in-progress operation this resolver finishes (merge, cherry-pick,
        /// or stash pop), so complete/abort dispatch correctly.
        kind: ops::ResolveKind,
        /// Conflicted file paths, parallel with `resolved`.
        files: Vec<String>,
        /// Whether each file has been staged as resolved.
        resolved: Vec<bool>,
        /// Cursor into `files`.
        file: usize,
        /// Parsed state of the file under the cursor, when it loaded and still
        /// has conflicts. `None` on an already-resolved file or a load error.
        current: Option<ResolverFile>,
    },
    /// A git operation (pull/push/fetch/delete/…) running on a background
    /// thread. Its result message is shown and the list refreshed when it
    /// finishes; `then` decides which view to reopen afterwards.
    Busy {
        label: String,
        rx: Task<Result<String, String>>,
        then: BusyThen,
    },
}

/// A conflicted file loaded into the resolver: its parsed contents plus the
/// resolution the user has chosen for each hunk.
pub struct ResolverFile {
    /// Parsed conflicted file (path, segments, ours/theirs labels).
    pub file: ConflictFile,
    /// Chosen action per conflict hunk, parallel with the file's `Hunk`
    /// segments; `None` until the user picks a side, so a file can't be staged
    /// with hunks left undecided.
    pub actions: Vec<Option<ResolutionAction>>,
    /// Cursor into the hunks (index over `Hunk` segments only). The detail
    /// pane auto-scrolls to keep this hunk in view.
    pub hunk: usize,
}

/// A minimal multi-line text editor, used for hand-editing a conflict hunk's
/// resolved text and for the commit dialog's message body. Lines are stored
/// without their trailing newline; the seed's trailing newline is remembered
/// and restored by `text` so line-based hunks round-trip exactly.
pub struct TextArea {
    /// The edited text, one entry per line, without line endings.
    pub lines: Vec<String>,
    /// Cursor row into `lines`.
    pub row: usize,
    /// Cursor column as a character index into the current line.
    pub col: usize,
    /// Whether the seed text ended in a newline, reapplied by `text`.
    trailing_newline: bool,
}

/// An empty editor still holds one (empty) line: `on_key` indexes the cursor's
/// line directly, so `lines` is never allowed to be empty.
impl Default for TextArea {
    fn default() -> Self {
        Self::new("")
    }
}

impl TextArea {
    /// Seeds the editor from `text`, splitting it into editable lines.
    pub fn new(text: &str) -> Self {
        let trailing_newline = text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        // A trailing newline leaves a final empty element; drop it so the cursor
        // does not sit on a phantom blank line below the content.
        if trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            row: 0,
            col: 0,
            trailing_newline,
        }
    }

    /// Reconstructs the edited text, restoring the seed's trailing newline.
    pub fn text(&self) -> String {
        let mut s = self.lines.join("\n");
        if self.trailing_newline {
            s.push('\n');
        }
        s
    }

    /// Number of characters on the current line.
    fn cur_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    /// Applies one key of editing (insert, delete, newline, or cursor move).
    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            // Single-line edits reuse the shared `line_*` primitives; the
            // multi-line cases (split on Enter, join across lines) stay here.
            KeyCode::Char(c) => {
                line_insert(&mut self.lines[self.row], &mut self.col, c);
            }
            KeyCode::Enter => {
                let b = char_byte(&self.lines[self.row], self.col);
                let rest = self.lines[self.row].split_off(b);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.col = 0;
            }
            KeyCode::Backspace => {
                if self.col > 0 {
                    line_backspace(&mut self.lines[self.row], &mut self.col);
                } else if self.row > 0 {
                    // Join this line onto the end of the previous one.
                    let cur = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.cur_len();
                    self.lines[self.row].push_str(&cur);
                }
            }
            KeyCode::Delete => {
                if self.col < self.cur_len() {
                    line_delete(&mut self.lines[self.row], self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&next);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.cur_len();
                }
            }
            KeyCode::Right => {
                if self.col < self.cur_len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    self.col = self.col.min(self.cur_len());
                }
            }
            KeyCode::Down => {
                if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = self.col.min(self.cur_len());
                }
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.cur_len(),
            _ => {}
        }
    }
}

/// Which view to reopen once a `View::Busy` operation completes. Most ops land
/// back on the worktree list, but stash/branch ops return to their manager so
/// the user can keep working there.
pub enum BusyThen {
    List,
    Stash(String),
    Branch,
    /// A fast-forward pull of the named worktree: a success lands on the list
    /// like `List`, but a non-fast-forward failure opens the rebase prompt
    /// instead of the error box.
    Pull {
        name: String,
    },
    /// After a backgrounded worktree removal succeeds, delete its branch on the
    /// main thread (so a refused delete can open the force prompt). Carries the
    /// worktree name and the branch to delete.
    DeleteBranch {
        name: String,
        branch: String,
    },
    /// After a merge/update/cherry-pick/stash-pop finishes, check the target for
    /// conflicts: open the resolver when any remain, otherwise report the clean
    /// result. Carries the worktree name, a label for what was applied, and the
    /// kind of operation so the resolver can finish it correctly.
    Resolve {
        target: String,
        source_label: String,
        kind: ops::ResolveKind,
    },
    /// A self-update finished installing: quit the TUI so `tui::run` can hand
    /// control to the freshly installed binary.
    Restart {
        exe: PathBuf,
    },
}

/// A worktree the picked commits can be cherry-picked into. Cherry-pick needs a
/// working directory, so targets are always existing worktrees.
pub struct CherryTarget {
    /// Worktree name (how it's addressed in ops).
    pub name: String,
    /// Branch checked out there, or None when detached.
    pub branch: Option<String>,
}

/// One line in the changed-files folder tree: either a folder that groups the
/// files beneath it, or a single changed file.
pub enum DiffRow {
    /// A folder. `prefix` is the full path from the worktree root ending in
    /// `/` (used to match the files under it); `label` is the last segment.
    Folder {
        prefix: String,
        label: String,
        depth: usize,
        /// True when the folder is collapsed: its files and subfolders are
        /// omitted from `rows` and the renderer shows a closed arrow.
        collapsed: bool,
    },
    /// A changed file; `index` points into the Diff view's `files`/`marked`.
    File {
        index: usize,
        label: String,
        depth: usize,
    },
}

/// Builds the folder-tree rows for the changed-file list. Files are sorted by
/// path so the tree reads top-down, and each folder row is emitted once, just
/// before the first file it contains. Folders whose prefix is in `collapsed`
/// are emitted as a single collapsed row with everything beneath them hidden.
pub fn build_diff_rows(files: &[StatusEntry], collapsed: &HashSet<String>) -> Vec<DiffRow> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| files[a].path.cmp(&files[b].path));
    let mut rows = Vec::new();
    // Directory segments currently "open" above the last file emitted.
    let mut stack: Vec<String> = Vec::new();
    for idx in order {
        let path = &files[idx].path;
        let parts: Vec<&str> = path.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        // Keep the shared prefix with the previous file's directories, open
        // folder rows for the rest.
        let mut common = 0;
        while common < stack.len() && common < dirs.len() && stack[common] == dirs[common] {
            common += 1;
        }
        stack.truncate(common);
        // True once any folder on the stack is collapsed: everything deeper
        // (subfolders and files) stays hidden until the stack pops above it.
        let mut hidden =
            (1..=stack.len()).any(|k| collapsed.contains(&format!("{}/", stack[..k].join("/"))));
        for d in &dirs[common..] {
            stack.push((*d).to_string());
            if hidden {
                continue;
            }
            let prefix = format!("{}/", stack.join("/"));
            let is_collapsed = collapsed.contains(&prefix);
            rows.push(DiffRow::Folder {
                prefix,
                label: (*d).to_string(),
                depth: stack.len() - 1,
                collapsed: is_collapsed,
            });
            hidden = is_collapsed;
        }
        if !hidden {
            rows.push(DiffRow::File {
                index: idx,
                label: parts[parts.len() - 1].to_string(),
                depth: dirs.len(),
            });
        }
    }
    rows
}

/// Builds a flat changed-file list: every file on its own row, labelled by its
/// full path (no folder grouping), sorted so the list reads top-down.
pub fn build_flat_rows(files: &[StatusEntry]) -> Vec<DiffRow> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| files[a].path.cmp(&files[b].path));
    order
        .into_iter()
        .map(|idx| DiffRow::File {
            index: idx,
            label: files[idx].path.clone(),
            depth: 0,
        })
        .collect()
}

/// Builds the changed-file rows in tree or flat layout per `tree`.
pub fn build_rows(files: &[StatusEntry], tree: bool, collapsed: &HashSet<String>) -> Vec<DiffRow> {
    if tree {
        build_diff_rows(files, collapsed)
    } else {
        build_flat_rows(files)
    }
}

/// Whether a porcelain status `code` marks a file that has no committed version
/// to revert to: untracked (`??`) or newly added to the index (`A`).
pub fn is_new_file(code: &str) -> bool {
    code.starts_with('?') || code.starts_with('A')
}

/// The `files` index for the row at `cursor`, or `None` when it is a folder.
pub fn current_file_index(rows: &[DiffRow], cursor: usize) -> Option<usize> {
    match rows.get(cursor) {
        Some(DiffRow::File { index, .. }) => Some(*index),
        _ => None,
    }
}

/// Which part of the commit dialog has keyboard focus.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitFocus {
    /// The changed-file list: ↑/↓ move, Space toggles, `a` toggles all.
    Files,
    /// The commit message input: typing edits the message.
    Message,
    /// The commit body: typing edits it and Enter inserts a newline, so
    /// Ctrl+S is what commits from here.
    Body,
}

/// Why a branch could not be safely deleted after its worktree was removed,
/// used to word the force-delete prompt.
pub enum ForceBranchReason {
    /// The branch has commits not merged anywhere (`git branch -d` refused).
    NotMerged,
    /// The branch is still checked out in another worktree (its name); forcing
    /// switches that worktree to the default branch first.
    CheckedOutElsewhere(String),
}

/// Which panel of the three-panel Worktrees tab owns the keyboard. The diff
/// pane never takes focus: it is scrolled remotely (Shift+arrows, J/K/H/L, or
/// the wheel over it), the same way the Changes tab scrolls it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorktreesFocus {
    /// The worktree list on top.
    #[default]
    List,
    /// The changed-file list at the bottom left (or the commit list when the
    /// selected worktree is clean).
    Files,
}

/// Bottom panel of the three-panel Worktrees layout when the selected worktree
/// has no uncommitted changes: that worktree's branch commit history (same
/// rows as `View::BranchCommits`, loaded via `branch_log_lines`).
#[derive(Debug, Clone)]
pub struct WorktreeCommitsPanel {
    /// Worktree whose branch history is shown.
    pub name: String,
    /// Branch used for the log title and `branch_log_lines` (or the worktree
    /// name when detached, with rows from `worktree_log_lines` instead).
    pub branch: String,
    /// True when `branch` is a real branch ref; false for detached HEAD.
    pub from_branch: bool,
    pub lines: Vec<GraphLine>,
    pub selected: usize,
}

/// The top-level tabs of the main window. `View::List` renders whichever tab
/// is active; overlays (create, diff, switch, …) draw on top of it and leave
/// the active tab intact when they close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Worktrees,
    Changes,
    Branches,
    Stash,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Worktrees,
        Tab::Changes,
        Tab::Branches,
        Tab::Stash,
        Tab::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Worktrees => "Worktrees",
            Tab::Changes => "Changes",
            Tab::Branches => "Branches",
            Tab::Stash => "Stash",
            Tab::Settings => "Settings",
        }
    }

    /// A single-character glyph shown before the tab's title in the tab bar.
    pub fn glyph(self) -> &'static str {
        match self {
            Tab::Worktrees => "⌂",
            Tab::Changes => "±",
            Tab::Branches => "⎇",
            Tab::Stash => "▤",
            Tab::Settings => "⚙",
        }
    }

    /// The next tab, wrapping at the end.
    pub fn next(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// The previous tab, wrapping at the start.
    pub fn prev(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Geometry of the active view's clickable row list, recorded by the renderer
/// each frame so a mouse click can be mapped back to a row index. `None` when
/// the active view has no clickable list (or an overlay covers it).
#[derive(Clone, Copy)]
pub struct RowList {
    /// Content rect where rows are drawn (inside the panel border/padding).
    pub inner: Rect,
    /// Rows of chrome inside `inner` above the first data row, e.g. a table
    /// header. Data row 0 is drawn at `inner.y + header`.
    pub header: u16,
    /// Index of the first visible row (the list's scroll offset).
    pub offset: usize,
    /// Total number of rows.
    pub len: usize,
}

/// Whether a click at `col`/`row` landed inside `rect`. Used for the click
/// targets the renderer records as bare rects (tab labels, the diff panel's path
/// title) rather than as a `RowList`.
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

impl RowList {
    /// Whether (`col`, `row`) is anywhere inside the list's content rect,
    /// chrome rows included. Used to route the wheel to the panel under the
    /// pointer, which `hit` is too strict for.
    fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.inner.x
            && col < self.inner.x + self.inner.width
            && row >= self.inner.y
            && row < self.inner.y + self.inner.height
    }

    /// Row index at screen position (`col`, `row`), or `None` when the click
    /// falls outside the list's data rows.
    fn hit(&self, col: u16, row: u16) -> Option<usize> {
        let top = self.inner.y + self.header;
        if col < self.inner.x
            || col >= self.inner.x + self.inner.width
            || row < top
            || row >= self.inner.y + self.inner.height
        {
            return None;
        }
        let idx = self.offset + (row - top) as usize;
        (idx < self.len).then_some(idx)
    }
}

/// State behind the Changes tab: a per-file changes browser for one worktree,
/// with a list of changed files on the left and the selected file's diff on the
/// right. Files can be marked for commit, stashed, or reverted from here.
/// Re-runs on a throttled timer (to catch edits made outside the app) and on
/// `r`.
pub struct ChangesTab {
    /// Worktree the changes belong to. Empty before the tab has been opened.
    pub name: String,
    /// Changed files, parallel with `marked`.
    pub files: Vec<StatusEntry>,
    /// Whether each file is selected for commit; defaults to all true.
    pub marked: Vec<bool>,
    /// Folder-tree rows derived from `files`, rebuilt whenever `files` changes.
    /// The cursor (`selected`) indexes into this, not `files`.
    pub rows: Vec<DiffRow>,
    /// Cursor into `rows`.
    pub selected: usize,
    /// Diff text for the file under the cursor (empty on a folder row).
    pub content: String,
    /// Path the current `content` reflects, so an auto-refresh of the same file
    /// can keep the diff on screen (no flicker) while a switch to a different
    /// file shows a loading placeholder until its diff arrives.
    pub content_path: Option<String>,
    /// Monotonic token bumped on every load; a background diff result is only
    /// accepted when its token still matches, so results from files the user has
    /// already navigated past are discarded.
    pub load_gen: u64,
    /// In-flight background diff load: (token, path, diff text). Diffs are
    /// computed off the UI thread so switching files never blocks the app.
    pub pending: Option<Task<(u64, String, String)>>,
    /// True while a load for a *different* file is in flight, so the UI can show
    /// "loading…" instead of the previous file's stale diff.
    pub loading_new: bool,
    pub scroll: u16,
    /// Horizontal offset into the diff pane (Shift+←/→ or H/L).
    pub h_scroll: u16,
    /// When the diff was last recomputed, used to throttle auto-refresh.
    pub last_refresh: Instant,
}

impl Default for ChangesTab {
    fn default() -> Self {
        ChangesTab {
            name: String::new(),
            files: Vec::new(),
            marked: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            content: String::new(),
            content_path: None,
            load_gen: 0,
            pending: None,
            loading_new: false,
            scroll: 0,
            h_scroll: 0,
            last_refresh: Instant::now(),
        }
    }
}

pub struct App {
    pub ctx: Ctx,
    pub worktrees: Vec<WorktreeInfo>,
    pub selected: usize,
    /// Cheap changed-file preview (`ops::status`, no diff content) for
    /// whichever worktree row is highlighted on the Worktrees tab.
    pub worktree_preview: Vec<StatusEntry>,
    /// Which `selected` index `worktree_preview` currently reflects, so it's
    /// only recomputed on an actual selection change, not every frame.
    pub preview_for: Option<usize>,
    /// Background `ops::status` for the Worktrees tab preview. The usize is the
    /// `selected` index the result belongs to; `poll_preview_load` drops
    /// results that no longer match so fast navigation never applies a stale
    /// list. Drained each tick, mirroring `branches_pending`.
    preview_pending: Option<(usize, Task<Vec<StatusEntry>>)>,
    /// First visible row of the changed-file preview, so a worktree with more
    /// changes than the panel is tall can be scrolled through in place.
    pub preview_scroll: usize,
    /// Geometry of the preview's file rows, recorded by the renderer each frame
    /// so clicks and the wheel can be resolved against it.
    pub preview_list: Option<RowList>,
    /// Active top-level tab. Only meaningful while `view` is `View::List`.
    pub tab: Tab,
    /// Whether the Worktrees tab is drawing three panels. The configured
    /// layout, unless the terminal is too short to fit it: the renderer
    /// recomputes it each frame (see `ui::draw`), and the key, mouse, and tab
    /// handlers read it so they always match what is on screen.
    pub three_panel: bool,
    /// Which panel of the three-panel Worktrees tab has the keyboard.
    pub worktrees_focus: WorktreesFocus,
    /// Geometry of the three-panel layout's changed-file rows, recorded by the
    /// renderer each frame so clicks and the wheel can be resolved against it.
    /// When the bottom panel shows commits instead, this is that list's geometry.
    pub files_list: Option<RowList>,
    /// Content of the Changes tab, populated by `open_changes_tab`. Also feeds
    /// the bottom two panels of the three-panel Worktrees layout.
    pub changes: ChangesTab,
    /// When `Some`, the three-panel bottom area shows this commit list instead
    /// of the changes/diff panels (selected worktree is clean).
    pub worktree_commits: Option<WorktreeCommitsPanel>,
    /// Background load for `worktree_commits`. The key identifies which
    /// worktree/branch/mode the result belongs to; `poll_commits_load` drops
    /// mismatches so ↑/↓ never applies another row's history.
    commits_pending: CommitsPending,
    /// Background `ops::status` for `refresh_diff` / async `load_changes`. The
    /// `u64` is a generation token, `String` the worktree name, and `bool`
    /// whether to preserve commit marks. Drained by `poll_status_refresh`.
    status_refresh_pending: StatusRefreshPending,
    /// Bumped whenever a status refresh or async `load_changes` starts; results
    /// with a stale token are ignored.
    status_refresh_gen: u64,
    /// Background `ops::stash_list` for the Stash tab. The `String` is the
    /// worktree name the result belongs to.
    stash_pending: StashPending,
    /// Branches shown on the Branches tab, loaded by `load_branches`. Kept
    /// on screen (stale) while a background reload is in flight, so
    /// switching back into the tab is instant instead of flashing empty.
    pub branches: Vec<BranchListItem>,
    /// Cursor into `branches` on the Branches tab.
    pub branch_selected: usize,
    /// A `load_branches` reload running on a background thread, so the
    /// (potentially many git invocations behind) branch list never blocks
    /// the tab switch. Drained by `poll_branches_load` each tick.
    branches_pending: Option<Task<Result<ops::BranchListResult, String>>>,
    /// The selection `poll_branches_load` should clamp and apply once the
    /// in-flight `branches_pending` load lands.
    branches_want_selected: usize,
    /// When the current `branches` cache was last filled. `None` means never
    /// loaded (or invalidated); the next `ensure_branches` will kick off a load.
    branches_loaded_at: Option<Instant>,
    /// Worktree the Stash tab's entries belong to. Empty before the tab has
    /// been opened.
    pub stash_name: String,
    /// Stash entries shown on the Stash tab, loaded by `reload_stash_tab`.
    pub stash_entries: Vec<StashEntry>,
    /// Cursor into `stash_entries` on the Stash tab.
    pub stash_selected: usize,
    /// The repo's `.wtm.toml` as edited on the Settings tab. Reloaded from disk
    /// every time the tab is entered; edits write immediately on change.
    pub settings: ConfigEditor,
    /// The screen currently on top and interacting with the user. Always the
    /// top of the navigation stack: `push_screen` moves it onto `stack` and puts
    /// a new screen here; `pop_screen` restores it from `stack`.
    pub view: View,
    /// Screens the user drilled through to reach `view`, oldest first. Popping
    /// `view` returns to `stack.last()`, so each screen returns to whoever opened
    /// it without carrying its own back-reference. The root worktree list is
    /// never kept here (an empty stack means `view` is at the root).
    pub stack: Vec<View>,
    /// The active modal overlay (confirm/prompt/hunk editor), drawn on top of
    /// `view` and handled by `on_modal_key` before any per-screen key handler.
    pub modal: Option<Modal>,
    /// Set by the renderer each frame; read by `on_mouse` to resolve clicks.
    pub row_list: Option<RowList>,
    /// Screen rect of each top-level tab label in the tab bar, recorded by the
    /// renderer so a click on one selects that tab. Empty when the bar is not
    /// on screen or something (modal, help, error) covers it.
    pub tab_hits: Vec<(Rect, Tab)>,
    /// Screen rect of the Changes tab's diff-panel path title, recorded by the
    /// renderer so a click there copies the path. `None` unless a file's diff is
    /// on screen.
    pub diff_path_hit: Option<Rect>,
    /// Column, row and time of the last left click, so a second click on the
    /// same cell soon after counts as a double click. Cleared once consumed, so
    /// three clicks are one double click plus a single, not two doubles.
    last_click: Option<(u16, u16, Instant)>,
    /// One-line status shown in the header. Auto-clears after a few seconds
    /// so it doesn't linger over the key hints.
    pub message: Option<String>,
    /// When the current `message` first appeared, plus the text it was set for,
    /// so a replaced message restarts the timer. Managed by `expire_message`.
    message_at: Option<Instant>,
    message_shown: Option<String>,
    /// A modal error, shown as a centered popup over everything else. Unlike
    /// `message`, it does not auto-expire; any key press dismisses it (see
    /// `on_key`).
    pub error: Option<String>,
    /// Scroll offset into the error popup's message, in rendered lines. A git
    /// error can run well past a screenful, so the popup scrolls rather than
    /// truncating; reset every time a new error is shown.
    pub error_scroll: u16,
    /// Largest useful `error_scroll`, recorded by the renderer each frame (it
    /// is the only place that knows the wrapped height and the popup's size).
    /// Keys clamp against it so scrolling stops at the last line.
    pub error_max_scroll: u16,
    /// Where new worktrees will be created, shown in the create dialog.
    pub worktree_base: Option<String>,
    /// Advances once per event-loop tick; drives the busy-overlay spinner.
    pub tick_count: u64,
    /// Whether commit history is drawn as a graph or a flat list, shared by the
    /// log and branch-commit views. Toggled with `t`.
    pub log_mode: LogMode,
    /// Whether changed-file lists group files under a folder tree (`true`) or
    /// list every file by its full path (`false`). Shared by the changes view
    /// and the commit browser; toggled with `t`.
    pub file_tree: bool,
    /// Folder prefixes (e.g. `src/tui/`) currently collapsed in the changed-
    /// file trees. Shared by the changes view and the commit browser so a
    /// collapse survives refreshes and view switches.
    pub collapsed_folders: HashSet<String>,
    /// When the list last reloaded itself (by timer or by `r`), used to pace
    /// `auto_refresh`.
    last_auto_refresh: Instant,
    /// When true, the help panel is drawn on top of the active view. It handles
    /// its own keys (tab switching, scrolling); anything else closes it and
    /// returns to the view underneath.
    pub show_help: bool,
    /// Which help tab is showing. Set from the active view each time help opens,
    /// so help lands on the page for whatever the user is looking at.
    pub help_tab: HelpTab,
    /// Scroll offset within the active help tab. Reset whenever the tab changes;
    /// clamped against the content at render time, as the diff and log views do.
    pub help_scroll: u16,
    /// An update check running on a background thread. Started at launch (when
    /// `auto_update_check` is on) and by the Settings tab's check-now row, so
    /// the network is never touched on the UI thread.
    update_check: Option<Task<Result<CheckOutcome, String>>>,
    /// Whether the in-flight check was asked for by the user, in which case its
    /// result is always reported, including "you're up to date". A launch check
    /// stays silent unless there is something to install.
    update_check_requested: bool,
    /// The newer release the last check found, kept so the Settings tab can
    /// keep showing it after the prompt is postponed.
    pub update_available: Option<Release>,
    /// Set once the update prompt has been shown, so postponing it isn't undone
    /// by the next tick reopening the same modal.
    update_prompted: bool,
    /// Set by a completed self-update: the binary `tui::run` should hand over to
    /// once the terminal is restored.
    pub restart_exe: Option<PathBuf>,
    /// Set by a `CommandMode::Terminal` open command: the shell command and the
    /// worktree directory to run it in. `tui::run` executes it after restoring
    /// the terminal, so an interactive program gets this terminal to itself.
    pub exec_on_exit: Option<(String, String)>,
    pub quit: bool,
    /// When set, q/Esc will not quit until this instant. Armed by back
    /// navigation so a duplicate key buffered while a slow tick/draw delayed
    /// the pop cannot quit the app as a side effect.
    ignore_quit_until: Option<Instant>,
}

impl App {
    pub fn new(ctx: Ctx) -> anyhow::Result<App> {
        let repo_root = ctx.repo_root.clone();
        let worktree_base = ctx
            .config
            .worktree_base(&ctx.repo_root)
            .ok()
            .map(|p| p.display().to_string());
        // An uninitialized repo opens into the setup wizard instead of the
        // worktree list; everything else waits until `.wtm.toml` exists.
        let initialized = setup::is_initialized(&ctx.repo_root);
        // The renderer refines this each frame (a short terminal falls back to
        // two panels); seed it so the first key press already agrees with the
        // configured layout.
        let config_three_panel = ctx.config.worktrees_layout() == WorktreesLayout::ThreePanel;
        let view = if initialized {
            View::List
        } else {
            View::Setup(Box::new(SetupWizard::new(ctx.repo_root.clone())))
        };
        let mut app = App {
            ctx,
            worktrees: Vec::new(),
            selected: 0,
            worktree_preview: Vec::new(),
            preview_for: None,
            preview_pending: None,
            preview_scroll: 0,
            preview_list: None,
            tab: Tab::Worktrees,
            three_panel: config_three_panel,
            worktrees_focus: WorktreesFocus::default(),
            files_list: None,
            changes: ChangesTab::default(),
            worktree_commits: None,
            commits_pending: None,
            status_refresh_pending: None,
            status_refresh_gen: 0,
            stash_pending: None,
            branches: Vec::new(),
            branch_selected: 0,
            branches_pending: None,
            branches_want_selected: 0,
            branches_loaded_at: None,
            stash_name: String::new(),
            stash_entries: Vec::new(),
            stash_selected: 0,
            // An uninitialized repo has no `.wtm.toml` to read yet, so the
            // editor starts empty; entering the tab reloads it either way.
            settings: ConfigEditor::load(repo_root.clone())
                .unwrap_or_else(|_| ConfigEditor::empty(repo_root)),
            view,
            stack: Vec::new(),
            modal: None,
            row_list: None,
            tab_hits: Vec::new(),
            diff_path_hit: None,
            last_click: None,
            message: None,
            message_at: None,
            message_shown: None,
            error: None,
            error_scroll: 0,
            error_max_scroll: 0,
            worktree_base,
            tick_count: 0,
            log_mode: LogMode::Tree,
            file_tree: true,
            collapsed_folders: HashSet::new(),
            last_auto_refresh: Instant::now(),
            show_help: false,
            help_tab: HelpTab::Basics,
            help_scroll: 0,
            update_check: None,
            update_check_requested: false,
            update_available: None,
            update_prompted: false,
            restart_exe: None,
            exec_on_exit: None,
            quit: false,
            ignore_quit_until: None,
        };
        if initialized {
            app.refresh();
        }
        // Apply the configured diff theme before the first frame so Settings and
        // the highlighter agree without waiting for a save.
        highlight::set_theme(app.ctx.config.diff_theme());
        // Fire and forget: the check runs on its own thread and its result is
        // drained by `tick`, so a slow or offline network never delays the
        // first frame. Never in unit tests, which must not reach the network.
        if !cfg!(test) && update::auto_check_enabled(&app.ctx.config) {
            app.start_update_check(false);
        }
        Ok(app)
    }

    /// Starts a background update check unless one is already running.
    /// `requested` marks a user-initiated check, which reports its result even
    /// when there is nothing to install.
    fn start_update_check(&mut self, requested: bool) {
        if self.update_check.is_some() {
            self.update_check_requested |= requested;
            return;
        }
        self.update_check_requested = requested;
        let (tx, rx) = channel();
        // Unit tests push a result through this channel by hand instead, so no
        // test run ever depends on the network.
        if !cfg!(test) {
            std::thread::spawn(move || {
                let _ = tx.send(update::check().map_err(|e| format!("{e:#}")));
            });
        }
        self.update_check = Some(Task::new(rx));
    }

    /// Drains a finished update check. A newer release opens the update prompt
    /// once the screen is free; a failed background check is swallowed, since an
    /// unattended check must never interrupt with an error.
    fn poll_update_check(&mut self) {
        let Some(task) = &self.update_check else {
            return;
        };
        let Some(result) = task.poll_latest() else {
            return;
        };
        self.update_check = None;
        let requested = std::mem::take(&mut self.update_check_requested);
        match result {
            Ok(CheckOutcome::Available(release)) => {
                if requested {
                    self.message = Some(format!("wtm {} is available", release.version));
                }
                self.update_available = Some(release);
                // A user-triggered check re-offers the prompt even if an earlier
                // one was postponed.
                if requested {
                    self.update_prompted = false;
                }
            }
            Ok(CheckOutcome::UpToDate { latest }) => {
                self.update_available = None;
                if requested {
                    self.message = Some(format!("wtm {latest} is the latest version"));
                }
            }
            Err(e) if requested => self.set_error(format!("update check failed: {e}")),
            Err(_) => {}
        }
    }

    /// Opens the update prompt once a newer release is known and the screen is
    /// free. Held back while a modal, error, or drill-in owns the screen so an
    /// update never interrupts work in progress.
    fn maybe_prompt_update(&mut self) {
        if self.update_prompted
            || self.modal.is_some()
            || self.error.is_some()
            || self.show_help
            || !matches!(self.view, View::List)
        {
            return;
        }
        let Some(release) = self.update_available.clone() else {
            return;
        };
        self.update_prompted = true;
        let body = vec![
            Line::from(format!(
                "wtm {} is available (you have {}).",
                release.version,
                update::CURRENT_VERSION
            )),
            Line::from(""),
            Line::from(format!("Release notes: {}", release.url)),
            Line::from(""),
            Line::from("Updating replaces this binary and relaunches wtm."),
        ];
        self.modal = Some(Modal::Confirm {
            title: format!("update to wtm {}", release.version),
            body,
            options: vec![
                ConfirmOption::new("update and restart").key('u'),
                ConfirmOption::new("not now").key('n'),
            ],
            selected: 0,
            action: ModalAction::UpdateApp(Box::new(release)),
        });
    }

    /// Downloads and installs `release` in the background, then quits so the
    /// new binary can take over. The binary being replaced is resolved up front
    /// so `BusyThen::Restart` knows what to hand control to.
    fn start_update_install(&mut self, release: Release) {
        let exe = match update::current_binary() {
            Ok(exe) => exe,
            Err(e) => return self.set_error(format!("{e:#}")),
        };
        let version = release.version.clone();
        self.start_busy(
            format!("installing wtm {version}"),
            BusyThen::Restart { exe },
            move |_ctx| {
                update::install(&release)
                    .map(|done| format!("updated to wtm {}", done.version))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Drills into `screen`, remembering the current one so `pop_screen`
    /// returns to it. Used for every navigation deeper than the worktree list
    /// (open a diff, a log, the commit browser, …).
    fn push_screen(&mut self, screen: View) {
        let prev = std::mem::replace(&mut self.view, screen);
        self.stack.push(prev);
    }

    /// Returns to whichever screen opened the current one, or the root worktree
    /// list when there is nothing left on the stack.
    fn pop_screen(&mut self) {
        self.view = self.stack.pop().unwrap_or(View::List);
    }

    /// Pops one screen and arms the quit-ignore window so a buffered duplicate
    /// q/Esc cannot quit once this back navigation lands.
    fn navigate_back(&mut self) {
        self.pop_screen();
        self.arm_ignore_quit();
    }

    /// Arms a short window during which q/Esc will not quit the app.
    fn arm_ignore_quit(&mut self) {
        self.ignore_quit_until = Some(Instant::now() + QUIT_IGNORE_AFTER_BACK);
    }

    /// Quits unless a recent back navigation armed the ignore window.
    fn request_quit(&mut self) {
        if self
            .ignore_quit_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        self.quit = true;
    }

    /// Jumps straight back to the root worktree list, discarding the whole
    /// stack. Used by the transient merge/resolve/setup flows, which always
    /// return to the list regardless of how they were reached.
    fn go_root(&mut self) {
        self.stack.clear();
        self.view = View::List;
    }

    /// Reloads the worktree list, keeping the selection in bounds.
    pub fn refresh(&mut self) {
        self.last_auto_refresh = Instant::now();
        match ops::list(&self.ctx) {
            Ok(wts) => {
                self.worktrees = wts;
                self.selected = self.selected.min(self.worktrees.len().saturating_sub(1));
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
        // Drop any in-flight preview so a result for the old list can't land
        // after this refresh; the next draw will kick off a fresh load.
        self.preview_for = None;
        self.preview_pending = None;
        // A delete (or external removal) can leave Changes/three-panel pointed
        // at a ghost name; realign before the next refresh_diff can not_found.
        self.realign_changes_after_list_reload();
    }

    /// If `changes.name` no longer exists in the worktree list, clear or
    /// retarget it so status/diff loads cannot surface `no worktree named…`.
    fn realign_changes_after_list_reload(&mut self) {
        if self.changes.name.is_empty() {
            return;
        }
        if self.worktrees.iter().any(|w| w.name == self.changes.name) {
            return;
        }
        let on_changes = self.tab == Tab::Changes;
        self.changes = ChangesTab::default();
        if on_changes {
            match self.worktrees.get(self.selected).map(|w| w.name.clone()) {
                Some(name) => {
                    let _ = self.load_changes(name);
                }
                None => self.tab = Tab::Worktrees,
            }
        }
    }

    /// Reloads the visible lists on a timer, so work done outside the app (an
    /// agent committing in a worktree, a branch landing upstream) shows up on
    /// its own.
    ///
    /// Deliberately conservative: it only runs on the plain list, never while an
    /// overlay, prompt, or modal error owns the screen, and it keeps the cursor
    /// on whatever it was on by name rather than by index. A failed reload is
    /// swallowed rather than raised, since an unattended background refresh
    /// should never interrupt with a popup; `r` still reports errors. The
    /// Branches tab uses its own `branches_refresh_mins` cache rather than
    /// reloading on this shorter interval.
    fn auto_refresh(&mut self) {
        // Never reload while an overlay, prompt, or modal owns the screen: a
        // confirm/prompt (e.g. naming a branch, confirming a delete) reads the
        // list under the cursor, so leave it alone until the user is done.
        if !matches!(self.view, View::List)
            || self.error.is_some()
            || self.modal.is_some()
            || self.last_auto_refresh.elapsed() < AUTO_REFRESH_INTERVAL
        {
            return;
        }
        self.last_auto_refresh = Instant::now();
        if let Ok(wts) = ops::list(&self.ctx) {
            let current = self.selected_worktree().map(|w| w.name.clone());
            self.worktrees = wts;
            self.selected = current
                .and_then(|name| self.worktrees.iter().position(|w| w.name == name))
                .unwrap_or(self.selected)
                .min(self.worktrees.len().saturating_sub(1));
            // Worktree paths/names may have shifted; invalidate the preview so
            // the next draw reloads status for the (possibly new) selection.
            self.preview_for = None;
            self.preview_pending = None;
        }
        // Branches use their own cache timeout (`branches_refresh_mins`); only
        // refresh them here when the cache has gone stale while the tab is open.
        if self.tab == Tab::Branches {
            self.ensure_branches(self.branch_selected);
        }
    }

    /// Key handling for the modal error popup: q/Esc (or Enter) dismiss it,
    /// everything else scrolls the message so a long error can be read in full.
    fn on_error_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                self.error = None;
                self.error_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll_error(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_error(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_error(10),
            KeyCode::PageUp => self.scroll_error(-10),
            KeyCode::Home | KeyCode::Char('g') => self.error_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.error_scroll = self.error_max_scroll,
            _ => {}
        }
    }

    /// Moves the error popup by `delta` lines, clamped to the message.
    fn scroll_error(&mut self, delta: i32) {
        let next = (self.error_scroll as i32 + delta).max(0) as u16;
        self.error_scroll = next.min(self.error_max_scroll);
    }

    /// Shows `msg` as a modal error popup (see `App::error`), scrolled to the
    /// top so a long message always starts at its first line.
    fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
        self.error_scroll = 0;
    }

    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.worktrees.get(self.selected)
    }

    /// Background work driven by the event loop's poll timeout: auto-refreshes
    /// the diff view and drains progress from an in-flight create.
    pub fn tick(&mut self) {
        // Advance the spinner clock every tick so the busy overlay keeps
        // animating even while a background op holds the screen.
        self.tick_count = self.tick_count.wrapping_add(1);
        self.expire_message();
        self.auto_refresh();
        // The update check is view-independent: drain it every tick so a check
        // that lands while the user is deep in a diff still prompts once they
        // come back to the list.
        self.poll_update_check();
        self.maybe_prompt_update();
        // Also view-independent: a branch list kicked off by `load_branches`
        // should land even if the user has since drilled into a branch's
        // commit history or another screen.
        self.poll_branches_load();
        // Same for the Worktrees tab's changed-file preview: status is loaded
        // off-thread so selection changes stay responsive.
        self.poll_preview_load();
        self.poll_commits_load();
        self.poll_status_refresh();
        self.poll_stash_load();
        if let View::Busy { rx, .. } = &self.view {
            if let Some(result) = rx.poll_latest() {
                // Pull the follow-up out of the view so we can mutate self, then
                // reopen whichever view this op should return to.
                let then = match std::mem::replace(&mut self.view, View::List) {
                    View::Busy { then, .. } => then,
                    _ => BusyThen::List,
                };
                // A success lands in the header's status line; a failure pops up
                // the modal error box instead, since git errors are often
                // multi-line and unreadable truncated to one line. The
                // DeleteBranch follow-up is special: on success it proceeds to
                // the (possibly force-prompting) branch delete rather than
                // showing a message here.
                match (result, then) {
                    // The new binary is in place: quit so `tui::run` can
                    // restore the terminal and hand over to it.
                    (Ok(m), BusyThen::Restart { exe }) => {
                        self.message = Some(m);
                        self.restart_exe = Some(exe);
                        self.quit = true;
                    }
                    (Ok(_), BusyThen::DeleteBranch { name, branch }) => {
                        self.refresh();
                        self.delete_branch_step(name, branch);
                    }
                    // A merge/update landed: open the resolver if it left
                    // conflicts, otherwise show its clean-result message.
                    (
                        Ok(m),
                        BusyThen::Resolve {
                            target,
                            source_label,
                            kind,
                        },
                    ) => {
                        self.refresh();
                        self.finish_merge_op(target, source_label, kind, m);
                    }
                    (Ok(m), then) => {
                        self.message = Some(m);
                        self.refresh();
                        match then {
                            // A pull or push started from the Changes tab moves
                            // the working tree under it, so re-read the file
                            // list and diff rather than showing a stale one.
                            BusyThen::List | BusyThen::Pull { .. } => {
                                if self.tab == Tab::Changes {
                                    self.refresh_diff();
                                }
                            }
                            BusyThen::DeleteBranch { .. }
                            | BusyThen::Resolve { .. }
                            | BusyThen::Restart { .. } => {}
                            BusyThen::Stash(name) => self.reload_stash_tab(name),
                            BusyThen::Branch => {
                                self.load_branches(self.branch_selected);
                            }
                        }
                    }
                    // A pull refused because the branch diverged gets a
                    // recovery prompt (retry with rebase) instead of the error.
                    (Err(e), BusyThen::Pull { name }) if git::is_non_fast_forward(&e) => {
                        self.refresh();
                        self.open_pull_rebase_modal(name);
                    }
                    (Err(e), _) => {
                        self.set_error(e);
                        self.refresh();
                    }
                }
            }
            return;
        }
        // The Changes tab folded away under the three-panel layout (a config
        // change, or a terminal that grew back to full height): move what the
        // user was looking at into the Worktrees tab's file panel.
        if self.tab_hidden(self.tab) && matches!(self.view, View::List) {
            self.tab = Tab::Worktrees;
            self.worktrees_focus = WorktreesFocus::Files;
        }
        // The Changes tab keeps its diff fresh while it is the visible screen;
        // a drill-in pushed on top of it (a log, a commit browser) owns the
        // tick instead, so the tab only polls while `view` is back at the root.
        // The three-panel Worktrees tab shows the same diff, so it polls and
        // refreshes on the same schedule.
        let diff_on_screen =
            self.tab == Tab::Changes || (self.tab == Tab::Worktrees && self.three_panel);
        if diff_on_screen && matches!(self.view, View::List) {
            self.poll_diff_load();
            if self.changes.last_refresh.elapsed() >= DIFF_REFRESH_INTERVAL {
                self.refresh_diff();
            }
            return;
        }
        if matches!(self.view, View::CommitDiff { .. } | View::StashDiff { .. }) {
            self.poll_file_browser_diff_load();
            return;
        }
        let View::Creating {
            lines,
            rx,
            done,
            outcome,
            ..
        } = &mut self.view
        else {
            return;
        };
        if *done {
            return;
        }
        let mut invalidate_branches = false;
        while let Some(msg) = rx.try_recv() {
            match msg {
                CreateMsg::Progress(line) => lines.push(line),
                CreateMsg::Done(Ok(result)) => {
                    for step in &result.setup {
                        let mark = if step.ok { "ok" } else { "FAILED" };
                        lines.push(format!("[{mark}] {}", step.step));
                        if let Some(detail) = &step.detail {
                            lines.push(format!("       {detail}"));
                        }
                    }
                    *outcome = Some(CreateOutcome {
                        ok: result.setup_ok,
                        path: result.path.clone(),
                        detail: None,
                    });
                    *done = true;
                    // Branch list may now include the new worktree's branch.
                    invalidate_branches = true;
                }
                CreateMsg::Done(Err(e)) => {
                    *outcome = Some(CreateOutcome {
                        ok: false,
                        path: String::new(),
                        detail: Some(e),
                    });
                    *done = true;
                }
            }
        }
        if invalidate_branches {
            self.invalidate_branches_cache();
        }
    }

    /// Starts (or restarts) the message timer when a new message appears and
    /// clears the message once it has been on screen past `MESSAGE_TIMEOUT`.
    fn expire_message(&mut self) {
        match &self.message {
            None => {
                self.message_at = None;
                self.message_shown = None;
            }
            Some(msg) => {
                if self.message_shown.as_deref() != Some(msg.as_str()) {
                    self.message_shown = Some(msg.clone());
                    self.message_at = Some(Instant::now());
                } else if self.message_at.map(|t| t.elapsed()) >= Some(MESSAGE_TIMEOUT) {
                    self.message = None;
                    self.message_at = None;
                    self.message_shown = None;
                }
            }
        }
    }

    /// True when the active view has a text field listening for characters, so
    /// `?` must reach it as a literal rather than opening help. F1 is the way in
    /// from these views.
    fn view_takes_text_input(&self) -> bool {
        // A prompt or the hunk editor listens for characters, so `?` must reach
        // it as a literal rather than opening help.
        if matches!(
            self.modal,
            Some(Modal::Prompt { .. } | Modal::HunkEditor(_) | Modal::FileEditor { .. })
        ) {
            return true;
        }
        match &self.view {
            // Row 0 with the base button unfocused and no picker open is the
            // new-branch name field, which doubles as the branch filter.
            View::Create {
                selected: 0,
                base_focus: false,
                base_pick: None,
                ..
            } => true,
            View::Commit {
                focus: CommitFocus::Message | CommitFocus::Body,
                ..
            } => true,
            View::Switch { .. } | View::RunCommand { .. } | View::RenameWorktree { .. } => true,
            View::Creating { done: false, .. } => true,
            View::List => self.tab == Tab::Settings && self.settings.is_typing(),
            View::Setup(wizard) => matches!(
                &wizard.step,
                setup::Step::ClonePath { .. }
                    | setup::Step::LocationCustom { .. }
                    | setup::Step::CopyFiles { .. }
                    | setup::Step::RunCommands { .. }
                    | setup::Step::Review {
                        editing: Some(_),
                        ..
                    }
            ),
            _ => false,
        }
    }

    /// Opens the help panel on the page documenting the active view.
    fn open_help(&mut self) {
        self.help_tab = HelpTab::for_view(&self.view, self.tab);
        self.help_scroll = 0;
        self.show_help = true;
    }

    fn set_help_tab(&mut self, tab: HelpTab) {
        self.help_tab = tab;
        self.help_scroll = 0;
    }

    /// Keys for the help panel: switch tabs, scroll, or close on anything else
    /// (Esc, q, ?, F1).
    fn on_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.set_help_tab(self.help_tab.next())
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.set_help_tab(self.help_tab.prev())
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
            _ => self.show_help = false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.message = None;
        // A modal error popup owns every key until it is dismissed, so nothing
        // reaches Ctrl+C handling or the view underneath. Only q/Esc close it:
        // a long git error is scrollable, and a stray key press must not throw
        // it away before it has been read.
        if self.error.is_some() {
            self.on_error_key(key);
            return;
        }
        // Ctrl+C: while setup runs it must be pressed twice to kill the
        // command; everywhere else it quits like q.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let View::Creating {
                done: false,
                control,
                kill_armed,
                lines,
                ..
            } = &mut self.view
            {
                if *kill_armed {
                    control.kill();
                    lines.push("killing setup command…".to_string());
                } else {
                    *kill_armed = true;
                    self.message =
                        Some("setup is running; press Ctrl+C again to kill it".to_string());
                }
            } else {
                self.quit = true;
            }
            return;
        }
        // The help panel is modal: it handles its own keys and everything else
        // closes it, returning to the view underneath.
        if self.show_help {
            self.on_help_key(key);
            return;
        }
        // Opening help is handled here rather than per-view so every view gets
        // it. `?` is a character a text field would type, so it only opens help
        // where nothing is listening for input; F1 works everywhere.
        if key.code == KeyCode::F(1)
            || (key.code == KeyCode::Char('?') && !self.view_takes_text_input())
        {
            self.open_help();
            return;
        }
        // A modal overlay captures keys before any per-screen handler.
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        match &mut self.view {
            View::List => self.on_list_key(key),
            View::Create { .. } => self.on_create_key(key),
            View::RunCommand { input, .. } => match key.code {
                KeyCode::Esc => self.navigate_back(),
                KeyCode::Enter => {
                    if let View::RunCommand { name, path, input } =
                        std::mem::replace(&mut self.view, View::List)
                    {
                        let cmd = input.trimmed();
                        if !cmd.is_empty() {
                            let (branch, status) = self
                                .worktrees
                                .iter()
                                .find(|w| w.name == name)
                                .map(|w| {
                                    (
                                        w.branch.clone().unwrap_or_default(),
                                        w.open_status().to_string(),
                                    )
                                })
                                .unwrap_or_default();
                            // A one-off command typed here has no saved mode,
                            // so it runs the safe way: detached, TUI stays up.
                            let cmd = OpenCommand::new(cmd);
                            self.spawn_open_command(&cmd, &name, &path, &branch, &status);
                        }
                    }
                }
                _ => {
                    input.on_key(key);
                }
            },
            View::RenameWorktree { input, .. } => match key.code {
                KeyCode::Esc => self.navigate_back(),
                KeyCode::Enter => {
                    if let View::RenameWorktree { name, input } =
                        std::mem::replace(&mut self.view, View::List)
                    {
                        let new = input.trimmed();
                        if new.is_empty() {
                            self.message = Some("new name must not be empty".to_string());
                        } else {
                            self.rename_worktree(name, new);
                        }
                    }
                }
                _ => {
                    input.on_key(key);
                }
            },
            View::Creating {
                done,
                control,
                input,
                kill_armed,
                lines,
                ..
            } => {
                if *done {
                    if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                        self.navigate_back();
                        self.refresh();
                    }
                    return;
                }
                // Any other key disarms a pending Ctrl+C kill.
                *kill_armed = false;
                match key.code {
                    KeyCode::Enter => {
                        let text = std::mem::take(input);
                        if control.send_line(&text) {
                            lines.push(format!("❯ {text}"));
                        } else {
                            lines.push("(no setup command is running to receive input)".into());
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                }
            }
            View::Setup(wizard) => match wizard.on_key(key, &mut self.message) {
                WizardOutcome::Quit => self.quit = true,
                WizardOutcome::Done => {
                    let draft = wizard.draft.clone();
                    self.finish_setup(&draft);
                }
                WizardOutcome::Continue => {}
            },
            View::Commit { .. } => self.on_commit_key(key),
            View::Switch { .. } => self.on_switch_key(key),
            View::Log { .. } => self.on_log_key(key),
            View::CommitDiff { .. } | View::StashDiff { .. } => self.on_file_browser_key(key),
            View::BranchCommits { .. } => self.on_branch_commits_key(key),
            View::CherryPick { .. } => self.on_cherry_pick_key(key),
            View::MergePick { .. } => self.on_merge_pick_key(key),
            View::RebasePick { .. } => self.on_rebase_pick_key(key),
            View::MoveChanges { .. } => self.on_move_changes_key(key),
            View::OpenCommand { .. } => self.on_open_command_key(key),
            View::UpstreamPick { .. } => self.on_upstream_pick_key(key),
            View::StashTarget { .. } => self.on_stash_target_key(key),
            View::ConflictResolver { .. } => self.on_resolver_key(key),
            // A background op owns the screen until tick() drains its result.
            View::Busy { .. } => {}
        }
    }

    /// Pushes a confirmation modal, highlighting the first enabled option.
    fn push_confirm(
        &mut self,
        title: impl Into<String>,
        body: Vec<Line<'static>>,
        options: Vec<ConfirmOption>,
        action: ModalAction,
    ) {
        let selected = options.iter().position(|o| o.enabled).unwrap_or(0);
        self.modal = Some(Modal::Confirm {
            title: title.into(),
            body,
            options,
            selected,
            action,
        });
    }

    /// Pushes a single-line prompt modal.
    fn push_prompt(
        &mut self,
        title: impl Into<String>,
        input: TextInput,
        hint: impl Into<String>,
        action: ModalAction,
    ) {
        self.modal = Some(Modal::Prompt {
            title: title.into(),
            input,
            hint: hint.into(),
            action,
        });
    }

    /// Key handling while a modal overlay is open. Navigation and text editing
    /// happen in place; a terminal key resolves the modal into a `ModalResult`,
    /// pops it, and routes the outcome to `dispatch_modal`.
    fn on_modal_key(&mut self, key: KeyEvent) {
        // The hunk editor edits in place and saves back into the resolver.
        if matches!(self.modal, Some(Modal::HunkEditor(_))) {
            self.on_hunk_editor_key(key);
            return;
        }
        // The whole-file editor writes straight to disk instead.
        if matches!(self.modal, Some(Modal::FileEditor { .. })) {
            self.on_file_editor_key(key);
            return;
        }
        let result = match self.modal.as_mut() {
            Some(Modal::Prompt { input, .. }) => match key.code {
                KeyCode::Esc => Some(ModalResult::Cancelled),
                KeyCode::Enter => Some(ModalResult::Submitted(input.trimmed())),
                _ => {
                    input.on_key(key);
                    None
                }
            },
            Some(Modal::Confirm {
                options, selected, ..
            }) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    // Step to the previous enabled option.
                    if let Some(prev) = (0..*selected).rev().find(|&i| options[i].enabled) {
                        *selected = prev;
                    }
                    None
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    if let Some(next) = (*selected + 1..options.len()).find(|&i| options[i].enabled)
                    {
                        *selected = next;
                    } else if key.code == KeyCode::Tab {
                        // Tab wraps to the first enabled option.
                        if let Some(first) = options.iter().position(|o| o.enabled) {
                            *selected = first;
                        }
                    }
                    None
                }
                KeyCode::Enter | KeyCode::Char('y') => Some(ModalResult::Confirmed(*selected)),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => {
                    Some(ModalResult::Cancelled)
                }
                KeyCode::Char(c) => options
                    .iter()
                    .position(|o| o.enabled && o.shortcut() == Some(c))
                    .map(ModalResult::Confirmed),
                _ => None,
            },
            _ => None,
        };
        if let Some(result) = result {
            let action = match self.modal.take() {
                Some(Modal::Confirm { action, .. }) | Some(Modal::Prompt { action, .. }) => action,
                _ => return,
            };
            self.dispatch_modal(action, result);
        }
    }

    /// Carries out a resolved modal's effect. The modal has already been popped;
    /// each action decides what `Confirmed`/`Submitted`/`Cancelled` mean and may
    /// open a follow-up modal or change the screen.
    fn dispatch_modal(&mut self, action: ModalAction, result: ModalResult) {
        match action {
            ModalAction::RevertFile => {
                if let (ModalResult::Confirmed(_), Some(e)) = (result, self.diff_cursor_file()) {
                    self.revert_file(e);
                }
            }
            ModalAction::DeleteFile => {
                if let (ModalResult::Confirmed(_), Some(e)) = (result, self.diff_cursor_file()) {
                    self.delete_file(e);
                }
            }
            ModalAction::DiscardAllChanges => {
                if let ModalResult::Confirmed(_) = result {
                    self.discard_all_changes();
                }
            }
            ModalAction::IgnorePath { file, pattern } => {
                if let ModalResult::Confirmed(idx) = result {
                    let p = if idx == 0 { file } else { pattern };
                    self.add_ignore(&p);
                }
            }
            ModalAction::ConfirmPullBase {
                branch,
                base,
                target,
            } => {
                let ModalResult::Confirmed(idx) = result else {
                    return;
                };
                if idx == 0 {
                    if let Err(e) = ops::update_branch(&self.ctx, &target) {
                        self.set_error(format!("{e:#}"));
                        return;
                    }
                    self.message = Some(format!("updated '{target}'"));
                }
                self.request_create_checked(branch, base);
            }
            ModalAction::ConfirmExisting {
                branch,
                base,
                path,
                existing_name,
            } => {
                let ModalResult::Confirmed(idx) = result else {
                    return;
                };
                match idx {
                    // Open the existing worktree.
                    0 => match existing_name {
                        Some(name) => self.open_changes_tab(name),
                        None => self.message = Some("that directory is not a worktree".to_string()),
                    },
                    // Replace: only stop to confirm when it holds real work.
                    1 => match ops::target_has_changes(&self.ctx, Path::new(&path)) {
                        Ok(true) => self.open_confirm_replace_changes(branch, base, path),
                        Ok(false) => self.replace_target(branch, base, &path),
                        Err(e) => self.set_error(format!("{e:#}")),
                    },
                    _ => {}
                }
            }
            ModalAction::ConfirmReplaceChanges { branch, base, path } => {
                if let ModalResult::Confirmed(0) = result {
                    self.replace_target(branch, base, &path);
                }
            }
            ModalAction::DeleteWorktree {
                name,
                dirty,
                branch,
            } => {
                if let ModalResult::Confirmed(idx) = result {
                    let delete_branch = idx == 1;
                    self.begin_delete(name, dirty, branch, delete_branch);
                }
            }
            ModalAction::DeleteWorktreeDirty {
                name,
                branch,
                delete_branch,
            } => {
                let ModalResult::Confirmed(idx) = result else {
                    return;
                };
                match idx {
                    // Stash: keep the work, then remove the now-clean folder.
                    0 => match ops::stash_worktree(&self.ctx, &name) {
                        Ok(()) => self.do_delete(name, branch, delete_branch, false),
                        Err(e) => {
                            self.set_error(format!("{e:#}"));
                            self.refresh();
                        }
                    },
                    // Discard: force-remove the folder, throwing changes away.
                    1 => self.do_delete(name, branch, delete_branch, true),
                    _ => {}
                }
            }
            ModalAction::UpdateStash { name } => {
                let ModalResult::Confirmed(idx) = result else {
                    return;
                };
                match idx {
                    0 => self.run_update(name, true),
                    1 => self.run_update(name, false),
                    _ => {}
                }
            }
            ModalAction::ForceBranch { branch } => match result {
                ModalResult::Confirmed(_) => {
                    match ops::force_delete_branch(&self.ctx, &branch) {
                        Ok(()) => {
                            self.message = Some(format!("deleted branch '{branch}' (forced)"))
                        }
                        Err(e) => self.set_error(format!("{e:#}")),
                    }
                    self.refresh();
                }
                ModalResult::Cancelled => {
                    self.message = Some(format!("kept branch '{branch}'"));
                    self.refresh();
                }
                _ => {}
            },
            ModalAction::PullRebase { name } => {
                if let ModalResult::Confirmed(_) = result {
                    self.start_pull_rebase(name);
                }
            }
            ModalAction::BranchCreate => {
                if let ModalResult::Submitted(name) = result {
                    if name.is_empty() {
                        self.message = Some("branch name must not be empty".to_string());
                    } else {
                        self.branch_create(name);
                    }
                }
            }
            ModalAction::BranchRename { old } => {
                if let ModalResult::Submitted(new) = result {
                    if new.is_empty() {
                        self.message = Some("branch name must not be empty".to_string());
                    } else {
                        self.branch_rename(old, new);
                    }
                }
            }
            ModalAction::BranchDelete { name } => match result {
                ModalResult::Confirmed(0) => self.branch_delete(name, false),
                ModalResult::Confirmed(_) => self.branch_delete(name, true),
                ModalResult::Submitted(_) | ModalResult::Cancelled => {}
            },
            ModalAction::StashPush { name } => {
                if let ModalResult::Submitted(msg) = result {
                    let msg = if msg.is_empty() { None } else { Some(msg) };
                    self.stash_push(name, msg);
                }
            }
            ModalAction::StashDrop { name, index } => {
                if let ModalResult::Confirmed(_) = result {
                    self.stash_action("drop", name, index);
                }
            }
            ModalAction::ResolverAbort => {
                if let ModalResult::Confirmed(_) = result {
                    self.abort_resolver();
                }
            }
            // Postponing (option 1, Esc, or n) leaves `update_available` set so
            // the Settings tab still shows it, but `update_prompted` keeps the
            // prompt from reappearing on the next tick.
            ModalAction::UpdateApp(release) => {
                if let ModalResult::Confirmed(0) = result {
                    self.start_update_install(*release);
                }
            }
        }
    }

    /// The `StatusEntry` under the Changes tab's cursor, or `None` on a folder
    /// row. Used by the revert/delete confirmations.
    fn diff_cursor_file(&self) -> Option<StatusEntry> {
        let c = &self.changes;
        current_file_index(&c.rows, c.selected).and_then(|i| c.files.get(i).cloned())
    }

    /// Confirmation for reverting the file under the Diff cursor.
    fn open_revert_modal(&mut self, path: String) {
        let body = vec![Line::from(format!("discard all changes to '{path}'?"))];
        let options = vec![ConfirmOption::new("discard changes").destructive()];
        self.push_confirm("revert file", body, options, ModalAction::RevertFile);
    }

    /// Confirmation for deleting the file under the Diff cursor.
    fn open_delete_file_modal(&mut self, path: String) {
        let body = vec![Line::from(format!("delete '{path}' from the worktree?"))];
        let options = vec![ConfirmOption::new("delete file").destructive()];
        self.push_confirm("delete file", body, options, ModalAction::DeleteFile);
    }

    /// Confirmation for discarding every uncommitted change in the Changes
    /// tab's worktree (tracked changes reset, untracked files removed).
    fn open_discard_all_modal(&mut self) {
        let name = self.changes.name.clone();
        let body = vec![Line::from(format!(
            "discard ALL uncommitted changes in '{name}'? this resets tracked files to HEAD and removes untracked files."
        ))];
        let options = vec![ConfirmOption::new("discard all changes").destructive()];
        self.push_confirm(
            "discard all changes",
            body,
            options,
            ModalAction::DiscardAllChanges,
        );
    }

    /// Prompt for adding a file or folder to `.gitignore`.
    fn open_ignore_modal(&mut self, file: String, pattern: String, is_folder: bool) {
        let (exact, glob) = if is_folder {
            ("just this folder", "all folders like it")
        } else {
            ("just this file", "all files like it")
        };
        let body = vec![Line::from("add to .gitignore:"), Line::from("")];
        let options = vec![
            ConfirmOption::new(format!("{exact}: {file}")),
            ConfirmOption::new(format!("{glob}: {pattern}")),
        ];
        self.push_confirm(
            "ignore",
            body,
            options,
            ModalAction::IgnorePath { file, pattern },
        );
    }

    /// Confirmation shown when a new-worktree target directory already exists.
    fn open_confirm_existing_modal(
        &mut self,
        branch: String,
        base: Option<String>,
        path: String,
        existing_name: Option<String>,
    ) {
        // Whichever screen triggered the create is dismissed; the modal sits
        // over the worktree list, and cancelling returns there.
        self.go_root();
        let is_wt = existing_name.is_some();
        let body = vec![
            Line::from(vec![
                Span::raw("a directory already exists at "),
                Span::styled(path.clone(), Style::new().bold()),
            ]),
            Line::from(""),
        ];
        let options = vec![
            ConfirmOption::new(match &existing_name {
                Some(n) => format!("open the existing worktree '{n}'"),
                None => "open (only if it is a worktree)".to_string(),
            })
            .enabled(is_wt),
            ConfirmOption::new("replace it (delete, then create)"),
            ConfirmOption::new("cancel"),
        ];
        self.push_confirm(
            "directory exists",
            body,
            options,
            ModalAction::ConfirmExisting {
                branch,
                base,
                path,
                existing_name,
            },
        );
    }

    /// Confirmation shown when replacing a directory would discard real work.
    fn open_confirm_replace_changes(&mut self, branch: String, base: Option<String>, path: String) {
        let body = vec![
            Line::from(vec![
                Span::raw("the worktree at "),
                Span::styled(path.clone(), Style::new().bold()),
            ]),
            Line::styled(
                "has changes that replacing it would permanently lose",
                Style::new().fg(theme::DANGER),
            ),
        ];
        let options = vec![
            ConfirmOption::new("force delete (lose all changes), then create").destructive(),
            ConfirmOption::new("cancel"),
        ];
        self.push_confirm(
            "changes would be lost",
            body,
            options,
            ModalAction::ConfirmReplaceChanges { branch, base, path },
        );
    }

    /// Delete confirmation for the selected worktree.
    fn open_delete_modal(&mut self, name: String, dirty: usize, branch: Option<String>) {
        let mut body = vec![Line::from(vec![
            Span::raw("remove worktree "),
            Span::styled(format!("'{name}'"), Style::new().bold()),
            Span::raw("?"),
        ])];
        if dirty > 0 {
            body.push(Line::styled(
                format!("⚠ {dirty} uncommitted change(s) will be lost — press f to force"),
                Style::new().fg(theme::DANGER),
            ));
        }
        let options = match &branch {
            Some(b) => vec![
                ConfirmOption::new(format!("remove folder only (keep branch '{b}')")),
                ConfirmOption::new(format!("remove folder and delete branch '{b}'")).destructive(),
            ],
            None => vec![ConfirmOption::new("remove the worktree folder")],
        };
        self.push_confirm(
            "delete",
            body,
            options,
            ModalAction::DeleteWorktree {
                name,
                dirty,
                branch,
            },
        );
    }

    /// Stash / discard / cancel prompt for removing a dirty worktree.
    fn open_delete_dirty_modal(
        &mut self,
        name: String,
        branch: Option<String>,
        delete_branch: bool,
    ) {
        let after = if delete_branch {
            "the folder and branch will be removed"
        } else {
            "the folder will be removed"
        };
        let body = vec![
            Line::from(vec![
                Span::raw("worktree "),
                Span::styled(format!("'{name}'"), Style::new().bold()),
                Span::raw(" has uncommitted changes"),
            ]),
            Line::styled(
                format!("choose what to do with them, then {after}"),
                Style::new().fg(theme::DANGER),
            ),
        ];
        let options = vec![
            ConfirmOption::new("stash the changes (keep them), then remove"),
            ConfirmOption::new("discard the changes and remove").destructive(),
            ConfirmOption::new("cancel"),
        ];
        self.push_confirm(
            "uncommitted changes",
            body,
            options,
            ModalAction::DeleteWorktreeDirty {
                name,
                branch,
                delete_branch,
            },
        );
    }

    /// Prompt before updating a worktree that has uncommitted changes.
    fn open_update_stash_modal(&mut self, name: String, dirty: usize) {
        let body = vec![
            Line::from(vec![
                Span::raw("worktree "),
                Span::styled(format!("'{name}'"), Style::new().bold()),
                Span::raw(format!(
                    " has {dirty} uncommitted change{}",
                    if dirty == 1 { "" } else { "s" }
                )),
            ]),
            Line::styled(
                "updating may conflict with them; how should they be handled?",
                Style::new().fg(theme::WARNING),
            ),
        ];
        let options = vec![
            ConfirmOption::new("stash them, update, then reapply (recommended)"),
            ConfirmOption::new("update without stashing"),
            ConfirmOption::new("cancel"),
        ];
        self.push_confirm(
            "update from default branch",
            body,
            options,
            ModalAction::UpdateStash { name },
        );
    }

    /// Force-delete prompt shown when a branch could not be safely removed.
    fn open_force_branch_modal(&mut self, branch: String, reason: ForceBranchReason) {
        self.go_root();
        let (warn, action) = match reason {
            ForceBranchReason::NotMerged => (
                format!("branch '{branch}' is not fully merged"),
                "force-delete it anyway (-D)".to_string(),
            ),
            ForceBranchReason::CheckedOutElsewhere(other) => (
                format!("branch '{branch}' is checked out in worktree '{other}'"),
                format!("switch '{other}' to the default branch, then delete '{branch}'"),
            ),
        };
        let body = vec![
            Line::from("the worktree folder was removed, but the branch was kept".dim()),
            Line::styled(format!("⚠ {warn}"), Style::new().fg(theme::DANGER)),
        ];
        let options = vec![ConfirmOption::new(action).key('f').destructive()];
        self.push_confirm(
            "delete branch?",
            body,
            options,
            ModalAction::ForceBranch { branch },
        );
    }

    /// Prompt to retry a refused fast-forward pull with a rebase.
    fn open_pull_rebase_modal(&mut self, name: String) {
        self.go_root();
        let body = vec![
            Line::styled(
                format!("⚠ '{name}' has diverged from its upstream"),
                Style::new().fg(theme::DANGER),
            ),
            Line::from("a plain fast-forward pull isn't possible".dim()),
        ];
        let options =
            vec![ConfirmOption::new("pull with rebase (replay local commits on top)").key('r')];
        self.push_confirm(
            "pull needs a rebase",
            body,
            options,
            ModalAction::PullRebase { name },
        );
    }

    /// Delete confirmation for the selected branch (`f` forces).
    fn open_branch_delete_modal(&mut self, name: String) {
        let body = vec![Line::from(format!("delete branch '{name}'?"))];
        let options = vec![
            ConfirmOption::new("delete branch"),
            ConfirmOption::new("force delete").key('f').destructive(),
        ];
        self.push_confirm(
            "delete branch",
            body,
            options,
            ModalAction::BranchDelete { name },
        );
    }

    /// Drop confirmation for the selected stash entry.
    fn open_stash_drop_modal(&mut self, name: String, index: Option<u32>) {
        let label = match index {
            Some(i) => format!("drop stash@{{{i}}}?"),
            None => "drop stash?".to_string(),
        };
        let body = vec![Line::from(label)];
        let options = vec![ConfirmOption::new("drop stash").destructive()];
        self.push_confirm(
            "drop stash",
            body,
            options,
            ModalAction::StashDrop { name, index },
        );
    }

    /// Abort confirmation for the conflict resolver.
    fn open_resolver_abort_modal(&mut self, target: String) {
        let body = vec![Line::from(format!(
            "abort the operation in '{target}' and discard resolutions?"
        ))];
        let options = vec![ConfirmOption::new("abort").destructive()];
        self.push_confirm("abort", body, options, ModalAction::ResolverAbort);
    }

    /// Reloads the merged config after a settings change and refreshes the
    /// cached worktree base shown in the create dialog.
    fn reload_config(&mut self) {
        match crate::config::Config::load(&self.ctx.repo_root) {
            Ok(config) => {
                self.ctx.config = config;
                self.worktree_base = self
                    .ctx
                    .config
                    .worktree_base(&self.ctx.repo_root)
                    .ok()
                    .map(|p| p.display().to_string());
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Writes the wizard's draft as `.wtm.toml`, reloads the config, and
    /// enters the normal list view. Errors keep the wizard open.
    fn finish_setup(&mut self, draft: &ConfigDraft) {
        let loaded = crate::settings::write_draft(&self.ctx.repo_root, draft)
            .and_then(|_| crate::config::Config::load(&self.ctx.repo_root));
        match loaded {
            Ok(config) => {
                self.ctx.config = config;
                self.worktree_base = self
                    .ctx
                    .config
                    .worktree_base(&self.ctx.repo_root)
                    .ok()
                    .map(|p| p.display().to_string());
                self.go_root();
                self.refresh();
                self.message = Some(format!("wrote {}", crate::config::CONFIG_FILE));
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Home-view key handling: cycle tabs, then dispatch to the active tab.
    fn on_list_key(&mut self, key: KeyEvent) {
        // Tab / Shift+Tab cycle the top-level tabs. (A prompt/confirm on the
        // Branches tab is a modal, handled by `on_modal_key` before reaching
        // here, so Tab is never captured mid-input.)
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.cycle_tab(key.code == KeyCode::Tab);
            return;
        }
        match self.tab {
            // In the three-panel layout the changed-file panel can hold the
            // keyboard instead of the worktree list above it.
            Tab::Worktrees if self.three_panel && self.worktrees_focus == WorktreesFocus::Files => {
                self.on_worktrees_files_key(key)
            }
            Tab::Worktrees => self.on_worktrees_tab_key(key),
            Tab::Changes => self.on_changes_tab_key(key),
            Tab::Branches => self.on_branches_tab_key(key),
            Tab::Stash => self.on_stash_tab_key(key),
            Tab::Settings => self.on_settings_tab_key(key),
        }
    }

    /// Cycles to the next (`forward`) or previous top-level tab, then runs
    /// whatever "on entry" loader that tab needs so its content isn't stale.
    fn cycle_tab(&mut self, forward: bool) {
        let step = |tab: Tab| if forward { tab.next() } else { tab.prev() };
        let mut next = step(self.tab);
        // The three-panel layout folds the Changes tab into the Worktrees tab,
        // so the cycle steps straight past it.
        if self.tab_hidden(next) {
            next = step(next);
        }
        self.select_tab(next);
    }

    /// Whether `tab` is currently folded away: the Changes tab is part of the
    /// Worktrees tab under the three-panel layout, so it is kept out of the tab
    /// bar and the Tab-key cycle.
    pub fn tab_hidden(&self, tab: Tab) -> bool {
        self.three_panel && tab == Tab::Changes
    }

    /// Switches to `tab` and runs its "on entry" loader, the same work
    /// `cycle_tab` does. Re-selecting the active tab still reloads it, matching
    /// what a click on the current tab implies (refresh what I'm looking at).
    pub fn select_tab(&mut self, tab: Tab) {
        // Under the three-panel layout the Changes tab has no tab of its own:
        // asking for it means focusing the changed-file panel on the Worktrees
        // tab, which `open_changes_tab` already knows how to do.
        if self.tab_hidden(tab) {
            self.tab = Tab::Worktrees;
            match self.selected_worktree().map(|w| w.name.clone()) {
                Some(name) => self.open_changes_tab(name),
                None => self.worktrees_focus = WorktreesFocus::Files,
            }
            return;
        }
        // Any other tab switch lands back on the worktree list.
        self.worktrees_focus = WorktreesFocus::List;
        self.tab = tab;
        match self.tab {
            Tab::Branches => self.ensure_branches(0),
            // Landing on Changes shows whichever worktree is highlighted on the
            // Worktrees tab. Coming back to the same worktree keeps the cursor
            // where it was and just re-reads the working tree.
            Tab::Changes => {
                if let Some(name) = self.selected_worktree().map(|w| w.name.clone()) {
                    if name == self.changes.name {
                        self.refresh_diff();
                    } else {
                        self.open_changes_tab(name);
                    }
                }
            }
            Tab::Stash => self.open_stash_tab(),
            Tab::Settings => self.open_settings_tab(),
            Tab::Worktrees => {}
        }
    }

    fn on_worktrees_tab_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            // Three-panel: the panel below the list is the diff, scrolled
            // remotely with the same keys the Changes tab uses.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) && self.three_panel => {
                self.scroll_diff(|s| s.saturating_sub(3));
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) && self.three_panel => {
                self.scroll_diff(|s| s.saturating_add(3));
            }
            KeyCode::Char('J') if self.three_panel => self.scroll_diff(|s| s.saturating_add(3)),
            KeyCode::Char('K') if self.three_panel => self.scroll_diff(|s| s.saturating_sub(3)),
            KeyCode::Char('L') if self.three_panel => self.scroll_diff_h(|s| s.saturating_add(8)),
            KeyCode::Char('H') if self.three_panel => self.scroll_diff_h(|s| s.saturating_sub(8)),
            // Shift+↑/↓ scrolls the changed-file preview below the table; the
            // plain arrows still move the worktree cursor.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.preview_scroll = self.preview_scroll.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.worktrees.len() {
                    self.selected += 1;
                    self.preview_scroll = 0;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.preview_scroll = 0;
                }
            }
            KeyCode::Char('r') => {
                self.refresh();
                self.message = Some("refreshed".to_string());
            }
            KeyCode::Char('n') => self.open_create(),
            KeyCode::Char('c') => self.open_commit(),
            KeyCode::Char('o') | KeyCode::Char('e') => self.run_open_command(),
            KeyCode::Char('s') => self.open_stash_tab(),
            KeyCode::Char('m') => self.open_move_changes_pick(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.start_push(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('b') => self.open_switch(),
            KeyCode::Char('u') => self.start_update(),
            KeyCode::Char('l') => self.open_log(),
            KeyCode::Char('R') => self.open_rename_worktree(),
            // Reopens the conflict resolver for a worktree left mid-merge,
            // mid-rebase, or mid-cherry-pick, including one started outside wtm.
            KeyCode::Char('x') => self.open_resolver_for_selected(),
            KeyCode::Char('d') => {
                if let Some(wt) = self.selected_worktree() {
                    if wt.is_main {
                        self.message = Some("cannot remove the main worktree".to_string());
                    } else {
                        let (name, dirty, branch) = (wt.name.clone(), wt.dirty, wt.branch.clone());
                        self.open_delete_modal(name, dirty, branch);
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(wt) = self.selected_worktree() {
                    let name = wt.name.clone();
                    self.open_changes_tab(name);
                }
            }
            _ => {}
        }
    }

    /// Keys while the changed-file panel of the three-panel Worktrees layout
    /// holds the focus. Identical to the Changes tab, except that q/Esc hand
    /// the keyboard back to the worktree list rather than quitting: the app
    /// only quits from the list, so focus is never a trap. When the selected
    /// worktree is clean the panel shows commits instead, with BranchCommits-
    /// style navigation.
    fn on_worktrees_files_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.worktrees_focus = WorktreesFocus::List;
            self.arm_ignore_quit();
            return;
        }
        if self.worktree_commits.is_some() {
            self.on_worktree_commits_key(key);
            return;
        }
        // Worktree-level commands the changed-file keymap leaves unbound, so
        // the file panel isn't a dead end for them either. Keys the file panel
        // binds to something else (u = revert, s/S = stash, c = commit,
        // t = layout, p/P = pull/push) are deliberately not in this list.
        if matches!(
            key.code,
            KeyCode::Char('f')
                | KeyCode::Char('b')
                | KeyCode::Char('l')
                | KeyCode::Char('m')
                | KeyCode::Char('R')
                | KeyCode::Char('x')
                | KeyCode::Char('d')
                | KeyCode::Char('n')
        ) {
            self.on_worktrees_tab_key(key);
            return;
        }
        self.on_changes_tab_key(key);
    }

    /// Keys for the three-panel commit list (clean worktree): move the cursor
    /// and open a commit's file browser, matching `View::BranchCommits`.
    fn on_worktree_commits_key(&mut self, key: KeyEvent) {
        let Some(panel) = &mut self.worktree_commits else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(i) = seek_commit_row(&panel.lines, panel.selected, true) {
                    panel.selected = i;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = seek_commit_row(&panel.lines, panel.selected, false) {
                    panel.selected = i;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                panel.selected = first_commit_row(&panel.lines);
            }
            KeyCode::Enter | KeyCode::Char('v') | KeyCode::Right => {
                self.open_commit_diff_from_worktree_commits();
            }
            KeyCode::Char('t') => self.toggle_log_mode(),
            // The panel belongs to the selected worktree, so anything it
            // doesn't bind itself falls through to the worktree list's keymap
            // (pull, push, fetch, update, commit, …). q/Esc never reach here:
            // `on_worktrees_files_key` intercepts them above.
            _ => self.on_worktrees_tab_key(key),
        }
    }

    /// Opens the commit browser for the row highlighted in the three-panel
    /// commits panel, viewed from that worktree (same as the worktree log).
    fn open_commit_diff_from_worktree_commits(&mut self) {
        let Some(panel) = &self.worktree_commits else {
            return;
        };
        let Some(entry) = panel
            .lines
            .get(panel.selected)
            .and_then(|l| l.entry.as_ref())
        else {
            return;
        };
        let name = panel.name.clone();
        let hash = entry.hash.clone();
        let label = format!(
            "{} {}",
            entry.hash.chars().take(9).collect::<String>(),
            entry.subject
        );
        self.open_commit_diff(name, hash, label);
    }

    /// Opens the rename prompt for the selected worktree, prefilled with its
    /// current name. Refuses the main worktree (it is the repository itself).
    fn open_rename_worktree(&mut self) {
        if let Some(wt) = self.selected_worktree() {
            if wt.is_main {
                self.message = Some("cannot rename the main worktree".to_string());
            } else {
                let name = wt.name.clone();
                self.push_screen(View::RenameWorktree {
                    input: TextInput::with_value(name.clone()),
                    name,
                });
            }
        }
    }

    /// Renames a worktree (its branch and directory), then refreshes the list
    /// and keeps the renamed worktree highlighted. Runs synchronously since the
    /// git operations are fast and local.
    fn rename_worktree(&mut self, name: String, new_name: String) {
        match ops::rename_worktree(&self.ctx, &name, &new_name) {
            Ok(r) => {
                self.message = Some(format!(
                    "renamed worktree '{}' to '{}'",
                    r.old_name, r.new_name
                ));
                self.refresh();
                if let Some(idx) = self.worktrees.iter().position(|w| w.name == r.new_name) {
                    self.selected = idx;
                }
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Switches to the Changes tab, loaded with the worktree named `name`.
    /// Under the three-panel layout those changes are already on screen below
    /// the worktree list, so this focuses that panel instead of switching tabs.
    fn open_changes_tab(&mut self, name: String) {
        if self.three_panel {
            // The panel follows the worktree cursor (`sync_three_panel_changes`),
            // so it usually holds `name` already; only a mismatch needs a load.
            if self.changes.name != name && !self.load_changes(name) {
                return;
            }
            self.worktrees_focus = WorktreesFocus::Files;
            return;
        }
        if self.load_changes(name) {
            self.tab = Tab::Changes;
        }
    }

    /// Points the changes pane (the Changes tab, or the bottom two panels of
    /// the three-panel Worktrees layout) at the worktree named `name`. Uses the
    /// cached Worktrees-tab preview when it already matches so Enter / tab
    /// switches never block on `ops::status`; otherwise status loads in the
    /// background. Returns false only when the name is immediately known to be
    /// missing from the worktree list.
    fn load_changes(&mut self, name: String) -> bool {
        if !self.worktrees.iter().any(|w| w.name == name) {
            self.set_error(format!("no worktree named '{name}'"));
            return false;
        }
        // Fast path: the Worktrees preview already holds this worktree's status.
        if self
            .preview_for
            .is_some_and(|i| self.worktrees.get(i).is_some_and(|w| w.name == name))
        {
            let files = self.worktree_preview.clone();
            self.apply_changes_files(name, files, false);
            return true;
        }
        self.start_status_refresh(name, false);
        true
    }

    /// Opens the Changes tab for `name` with the cursor on `path`. Used by the
    /// Worktrees tab's preview: clicking a changed file there jumps straight to
    /// that file's diff. A path that no longer shows up as changed (the status
    /// is re-read on the way in) just leaves the cursor at the top.
    fn open_changes_tab_at(&mut self, name: String, path: &str) {
        self.open_changes_tab(name.clone());
        // Status may have failed, leaving the pane on another worktree.
        if self.changes.name != name {
            return;
        }
        let Some(file) = self.changes.files.iter().position(|f| f.path == path) else {
            return;
        };
        // The file may sit inside a collapsed folder, which leaves it with no
        // row at all. Expand its ancestors so the cursor has somewhere to land.
        let segments: Vec<&str> = path.split('/').collect();
        let mut prefix = String::new();
        let mut expanded = false;
        for dir in &segments[..segments.len() - 1] {
            prefix.push_str(dir);
            prefix.push('/');
            expanded |= self.collapsed_folders.remove(&prefix);
        }
        if expanded {
            self.changes.rows =
                build_rows(&self.changes.files, self.file_tree, &self.collapsed_folders);
        }
        let Some(row) = self
            .changes
            .rows
            .iter()
            .position(|r| matches!(r, DiffRow::File { index, .. } if *index == file))
        else {
            return;
        };
        self.changes.selected = row;
        self.load_diff_content(true);
    }

    /// Loads the diff text for the file under the cursor into the Diff view.
    /// When the cursor sits on a folder row there is no diff to show, so the
    /// content is cleared. `reset_scroll` sends the viewport back to the top
    /// (used when the selected file changes); otherwise the current scroll is
    /// kept and merely clamped to the new content, so the periodic auto-refresh
    /// doesn't yank the user back to the top of the file they're reading.
    fn load_diff_content(&mut self, reset_scroll: bool) {
        let c = &mut self.changes;
        let entry = current_file_index(&c.rows, c.selected).and_then(|i| c.files.get(i).cloned());
        let name = c.name.clone();
        // A folder (or empty) row has no diff; clear it synchronously and cancel
        // any in-flight file load so its late result can't overwrite the blank.
        let Some(e) = entry else {
            c.content.clear();
            c.content_path = None;
            c.pending = None;
            c.loading_new = false;
            if reset_scroll {
                c.scroll = 0;
                c.h_scroll = 0;
            }
            return;
        };
        let path = e.path.clone();
        let untracked = e.code.starts_with('?');
        // Bump the generation, decide whether this is a switch to a new file
        // (so the UI shows a placeholder) or a same-file refresh (keep the diff
        // on screen to avoid flicker), and reset scroll now if we're switching.
        c.load_gen = c.load_gen.wrapping_add(1);
        let token = c.load_gen;
        let is_new = c.content_path.as_deref() != Some(path.as_str());
        if reset_scroll {
            c.scroll = 0;
            c.h_scroll = 0;
        }
        // Compute the diff off the UI thread; the result is picked up in `tick`
        // via `poll_diff_load` and applied only if its generation still matches.
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let path_for_thread = path.clone();
        std::thread::spawn(move || {
            let content = match ops::file_diff(&ctx, &name, &path_for_thread, untracked) {
                Ok(c) => c,
                Err(err) => format!("error: {err:#}"),
            };
            let _ = tx.send((token, path_for_thread, content));
        });
        self.changes.pending = Some(Task::new(rx));
        self.changes.loading_new = is_new;
    }

    /// Applies the newest background diff result to the Diff view, if one has
    /// arrived and still matches the current generation. Called each tick so a
    /// diff computed off the UI thread lands without blocking navigation.
    fn poll_diff_load(&mut self) {
        let c = &mut self.changes;
        let Some(rx) = &c.pending else {
            return;
        };
        let token = c.load_gen;
        // Drain to the most recent message so a burst of fast navigation doesn't
        // apply stale intermediate diffs.
        let Some((g, path, content)) = rx.poll_latest() else {
            return;
        };
        if g != token {
            return;
        }
        c.content = content;
        c.content_path = Some(path);
        c.pending = None;
        c.loading_new = false;
        // Don't let a shrunken diff leave the viewport past the last line.
        let max = c.content.lines().count().saturating_sub(1) as u16;
        c.scroll = c.scroll.min(max);
        let max_h = c
            .content
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0) as u16;
        c.h_scroll = c.h_scroll.min(max_h);
    }

    /// Handles mouse input. The scroll wheel moves the help, diff, or log
    /// viewport; other mouse events are ignored. In the changes view and the
    /// commit browser the wheel is panel-aware: over the changed-file list it
    /// moves the file cursor like the arrow keys, elsewhere it scrolls the
    /// diff text.
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        // A left click moves the selection to the clicked row, mirroring the
        // arrow keys. The help panel is modal, so a click on the view behind it
        // must not move that view's cursor.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if !self.show_help {
                let double = self.take_double_click(mouse.column, mouse.row);
                self.on_click(mouse.column, mouse.row);
                // The first click of the pair has already moved the cursor onto
                // the row, so the second one acts on whatever is selected now.
                if double {
                    self.on_double_click();
                }
            }
            return;
        }
        let down = match mouse.kind {
            MouseEventKind::ScrollDown => true,
            MouseEventKind::ScrollUp => false,
            _ => return,
        };
        // Scroll three lines per wheel notch, matching Shift+Up/Down.
        let delta = |s: u16| {
            if down {
                s.saturating_add(3)
            } else {
                s.saturating_sub(3)
            }
        };
        if self.show_help {
            self.help_scroll = delta(self.help_scroll);
            return;
        }
        // Whether the pointer sits over the active view's row list (the
        // changed-file panel in the diff views), per the geometry the renderer
        // recorded last frame.
        let over_list = self
            .row_list
            .is_some_and(|rl| rl.contains(mouse.column, mouse.row));
        // Worktrees tab: the wheel steps the worktree cursor over the table and
        // scrolls the changed-file preview over the panel below it.
        if matches!(self.view, View::List) && self.tab == Tab::Worktrees && self.three_panel {
            // Three panels, three destinations: the file (or commit) rows step
            // that cursor, the worktree rows step the worktree cursor, and
            // anywhere else (the diff) scrolls the diff text.
            if self
                .files_list
                .is_some_and(|rl| rl.contains(mouse.column, mouse.row))
            {
                if let Some(panel) = &mut self.worktree_commits {
                    if down {
                        if let Some(i) = seek_commit_row(&panel.lines, panel.selected, true) {
                            panel.selected = i;
                        }
                    } else if let Some(i) = seek_commit_row(&panel.lines, panel.selected, false) {
                        panel.selected = i;
                    }
                } else {
                    let c = &mut self.changes;
                    let moved = if down {
                        (c.selected + 1 < c.rows.len()).then(|| c.selected += 1)
                    } else {
                        (c.selected > 0).then(|| c.selected -= 1)
                    };
                    if moved.is_some() {
                        self.load_diff_content(true);
                    }
                }
            } else if over_list {
                if down {
                    if self.selected + 1 < self.worktrees.len() {
                        self.selected += 1;
                    }
                } else if self.selected > 0 {
                    self.selected -= 1;
                }
            } else if self.worktree_commits.is_none() {
                self.changes.scroll = delta(self.changes.scroll);
            }
            return;
        }
        if matches!(self.view, View::List) && self.tab == Tab::Worktrees {
            if self
                .preview_list
                .is_some_and(|rl| rl.contains(mouse.column, mouse.row))
            {
                self.preview_scroll = if down {
                    self.preview_scroll.saturating_add(3)
                } else {
                    self.preview_scroll.saturating_sub(3)
                };
            } else if over_list {
                if down {
                    if self.selected + 1 < self.worktrees.len() {
                        self.selected += 1;
                        self.preview_scroll = 0;
                    }
                } else if self.selected > 0 {
                    self.selected -= 1;
                    self.preview_scroll = 0;
                }
            }
            return;
        }
        // Over the Changes tab's file list: one file-cursor step per wheel
        // notch; anywhere else on that tab the wheel scrolls the diff text.
        if matches!(self.view, View::List) && self.tab == Tab::Changes {
            let c = &mut self.changes;
            if over_list {
                let moved = if down {
                    (c.selected + 1 < c.rows.len()).then(|| c.selected += 1)
                } else {
                    (c.selected > 0).then(|| c.selected -= 1)
                };
                if moved.is_some() {
                    self.load_diff_content(true);
                }
            } else {
                c.scroll = delta(c.scroll);
            }
            return;
        }
        match &mut self.view {
            View::CommitDiff { rows, selected, .. } | View::StashDiff { rows, selected, .. }
                if over_list =>
            {
                let moved = if down {
                    (*selected + 1 < rows.len()).then(|| *selected += 1)
                } else {
                    (*selected > 0).then(|| *selected -= 1)
                };
                if moved.is_some() {
                    self.load_file_browser_diff_content(true);
                }
            }
            View::CommitDiff { scroll, .. } | View::StashDiff { scroll, .. } => {
                *scroll = delta(*scroll)
            }
            // The log has no free scroll offset any more; the wheel steps the
            // commit cursor instead, matching the arrow keys.
            View::Log {
                lines, selected, ..
            } => {
                if let Some(next) = seek_commit_row(lines, *selected, down) {
                    *selected = next;
                }
            }
            _ => {}
        }
    }

    /// How close together two clicks on the same cell have to be to count as a
    /// double click. Generous enough for a deliberate double click without
    /// turning two unrelated clicks on one row into one.
    const DOUBLE_CLICK: Duration = Duration::from_millis(450);

    /// Whether this click completes a double click on the same cell, consuming
    /// the pair so a third click starts over.
    fn take_double_click(&mut self, col: u16, row: u16) -> bool {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(c, r, at)| c == col && r == row && now - at < Self::DOUBLE_CLICK);
        self.last_click = if double { None } else { Some((col, row, now)) };
        double
    }

    /// A double click activates whatever the cursor now sits on: on the Changes
    /// tab that opens the file (or expands the folder), matching Enter.
    fn on_double_click(&mut self) {
        if !matches!(self.view, View::List) {
            return;
        }
        // The three-panel layout's file panel is the Changes tab's list, so a
        // double click there activates the row the same way. On a clean
        // worktree that panel is the commit list: open the commit browser.
        let on_files = self.tab == Tab::Changes
            || (self.tab == Tab::Worktrees
                && self.three_panel
                && self.worktrees_focus == WorktreesFocus::Files);
        if on_files {
            if self.worktree_commits.is_some() {
                self.open_commit_diff_from_worktree_commits();
            } else {
                self.activate_changes_row();
            }
        }
    }

    /// Enter (or a double click) on a Changes-tab row: a folder row toggles
    /// open/closed, a file row hands the file to the OS's default application.
    fn activate_changes_row(&mut self) {
        let c = &self.changes;
        if matches!(c.rows.get(c.selected), Some(DiffRow::Folder { .. })) {
            self.tree_nav(KeyCode::Enter);
            return;
        }
        self.open_selected_file();
    }

    /// Opens the changed file under the cursor with the OS default application
    /// for its type. The worktree's own copy is opened, not the main repo's.
    fn open_selected_file(&mut self) {
        let c = &self.changes;
        let Some(rel) = current_file_index(&c.rows, c.selected)
            .and_then(|i| c.files.get(i))
            .map(|f| f.path.clone())
        else {
            return;
        };
        let name = c.name.clone();
        let root = match ops::path(&self.ctx, &name) {
            Ok(root) => root,
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        let full = Path::new(&root).join(&rel);
        // A deleted file has nothing left to open; say so rather than letting the
        // OS handler fail with its own wording.
        if !full.exists() {
            self.message = Some(format!("'{rel}' no longer exists in the worktree"));
            return;
        }
        match platform::open_path(&full) {
            Ok(()) => self.message = Some(format!("opened '{rel}'")),
            Err(e) => self.set_error(format!("cannot open '{rel}': {e:#}")),
        }
    }

    /// Copies the path shown in the diff panel's title to the system clipboard.
    /// Relative to the worktree root, which is what a path is useful as here.
    fn copy_diff_path(&mut self) {
        let c = &self.changes;
        let Some(path) = current_file_index(&c.rows, c.selected)
            .and_then(|i| c.files.get(i))
            .map(|f| f.path.clone())
        else {
            return;
        };
        match platform::copy_to_clipboard(&path) {
            Ok(()) => self.message = Some(format!("copied '{path}' to the clipboard")),
            Err(e) => self.set_error(format!("cannot copy to the clipboard: {e:#}")),
        }
    }

    /// Selects the list row under a left click, if one landed on the active
    /// view's clickable list. Loads the diff for a newly selected file so a
    /// click behaves exactly like arrowing onto the row.
    fn on_click(&mut self, col: u16, row: u16) {
        // The tab bar sits above every list, and the renderer only records its
        // geometry when nothing covers it, so this can be resolved first.
        if let Some((_, tab)) = self
            .tab_hits
            .iter()
            .find(|(rect, _)| rect_contains(*rect, col, row))
            .copied()
        {
            self.select_tab(tab);
            return;
        }
        // The diff panel's path title is click-to-copy. Resolved before the row
        // lists because it sits on a panel border, not in any of them.
        if self
            .diff_path_hit
            .is_some_and(|rect| rect_contains(rect, col, row))
        {
            self.copy_diff_path();
            // Don't let the same click also register as half of a double click on
            // the title, which would then try to open the file.
            self.last_click = None;
            return;
        }
        // Three-panel layout: a click in the changed-file (or commit) panel
        // takes the focus (the mouse equivalent of Enter) and lands the cursor.
        if let Some(idx) = self.files_list.and_then(|rl| rl.hit(col, row)) {
            self.worktrees_focus = WorktreesFocus::Files;
            if let Some(panel) = &mut self.worktree_commits {
                if idx < panel.lines.len() {
                    panel.selected = idx;
                }
            } else {
                let c = &mut self.changes;
                if idx < c.rows.len() && c.selected != idx {
                    c.selected = idx;
                    self.load_diff_content(true);
                }
            }
            return;
        }
        // A click on a changed file in the Worktrees tab's preview opens that
        // file's diff on the Changes tab, the mouse equivalent of Enter.
        if let Some(idx) = self.preview_list.and_then(|rl| rl.hit(col, row)) {
            let target = self
                .worktree_preview
                .get(idx)
                .map(|e| e.path.clone())
                .zip(self.selected_worktree().map(|w| w.name.clone()));
            if let Some((path, name)) = target {
                self.open_changes_tab_at(name, &path);
            }
            return;
        }
        let Some(idx) = self.row_list.and_then(|rl| rl.hit(col, row)) else {
            return;
        };
        // A confirm modal sits on top of everything else, so a hit while one
        // is open always targets its options, not the view underneath. Only
        // enabled options are selectable, matching keyboard nav's skip-logic.
        if let Some(Modal::Confirm {
            options, selected, ..
        }) = &mut self.modal
        {
            if idx < options.len() && options[idx].enabled {
                *selected = idx;
            }
            return;
        }
        match self.view {
            View::List => match self.tab {
                Tab::Worktrees => {
                    // A click on the worktree table takes the focus back from
                    // the three-panel layout's file panel.
                    self.worktrees_focus = WorktreesFocus::List;
                    if idx < self.worktrees.len() && self.selected != idx {
                        self.selected = idx;
                        self.preview_scroll = 0;
                    }
                }
                // The branch table's rows include group headings, so a click
                // index is a display row, not a branch index.
                Tab::Branches => self.select_branch_row(idx),
                Tab::Changes => {
                    let c = &mut self.changes;
                    if idx >= c.rows.len() || c.selected == idx {
                        return;
                    }
                    c.selected = idx;
                    self.load_diff_content(true);
                }
                Tab::Stash => {
                    if idx < self.stash_entries.len() {
                        self.stash_selected = idx;
                    }
                }
                // Non-uniform layout: section headers, spaced setting blocks
                // (value + description + blank), theme sample, and footer
                // lines, so `row_at_line` owns the decoding.
                Tab::Settings
                    if self.settings.editing.is_none() && self.settings.open_list.is_none() =>
                {
                    if let Some(row) = config_editor::row_at_line(idx) {
                        self.settings.selected = row;
                    }
                }
                Tab::Settings => {}
            },
            View::CommitDiff { .. } | View::StashDiff { .. } => {
                if let View::CommitDiff { selected, rows, .. }
                | View::StashDiff { selected, rows, .. } = &mut self.view
                {
                    if idx >= rows.len() || *selected == idx {
                        return;
                    }
                    *selected = idx;
                }
                self.load_file_browser_diff_content(true);
            }
            View::Commit { .. } => {
                if let View::Commit {
                    cursor,
                    focus,
                    files,
                    ..
                } = &mut self.view
                    && idx < files.len()
                {
                    *cursor = idx;
                    *focus = CommitFocus::Files;
                }
            }
            // Commit lists: a click lands the cursor on a commit row (art-only
            // graph rows are not selectable, matching the arrow keys).
            View::Log { .. } => {
                if let View::Log {
                    lines, selected, ..
                } = &mut self.view
                    && lines.get(idx).is_some_and(|l| l.entry.is_some())
                {
                    *selected = idx;
                }
            }
            View::BranchCommits { .. } => {
                if let View::BranchCommits {
                    lines, selected, ..
                } = &mut self.view
                    && lines.get(idx).is_some_and(|l| l.entry.is_some())
                {
                    *selected = idx;
                }
            }
            // The switch picker's cursor indexes the filtered list, which is
            // exactly what the row list reports.
            View::Switch { .. } => {
                if let View::Switch { selected, .. } = &mut self.view {
                    *selected = idx;
                }
            }
            View::CherryPick { .. } => {
                if let View::CherryPick {
                    targets, selected, ..
                } = &mut self.view
                    && idx < targets.len()
                {
                    *selected = idx;
                }
            }
            View::MergePick { .. } => {
                if let View::MergePick {
                    targets, selected, ..
                } = &mut self.view
                    && idx < targets.len()
                {
                    *selected = idx;
                }
            }
            View::MoveChanges { .. } => {
                if let View::MoveChanges {
                    targets, selected, ..
                } = &mut self.view
                    && idx < targets.len()
                {
                    *selected = idx;
                }
            }
            View::OpenCommand { .. } => {
                if let View::OpenCommand {
                    commands, selected, ..
                } = &mut self.view
                    && idx < commands.len()
                {
                    *selected = idx;
                }
            }
            View::StashTarget { .. } => {
                if let View::StashTarget {
                    targets, selected, ..
                } = &mut self.view
                    && idx < targets.len()
                {
                    *selected = idx;
                }
            }
            View::ConflictResolver { .. } => {
                if let View::ConflictResolver { files, file, .. } = &mut self.view {
                    if idx >= files.len() || *file == idx {
                        return;
                    }
                    *file = idx;
                }
                self.load_resolver_file();
            }
            // The wizard's rows are drawn per-step, so the click target
            // depends on which step (and, for Review, whether it's mid-edit)
            // is currently on screen.
            View::Setup(ref mut wizard) => match &mut wizard.step {
                setup::Step::Welcome { selected } => {
                    if idx < setup::WELCOME_OPTIONS.len() {
                        *selected = idx;
                    }
                }
                setup::Step::Location { selected } => *selected = idx,
                setup::Step::CloneBrowse { browser, .. } => {
                    if idx < browser.entries.len() {
                        browser.selected = idx;
                    }
                }
                // The rows aren't drawn one-per-line (a blank separator sits
                // before the write row), so the raw row index is decoded the
                // same way `draw_review` laid the lines out.
                setup::Step::Review {
                    selected,
                    editing: None,
                } => {
                    if idx < setup::REVIEW_ROWS - 1 {
                        *selected = idx;
                    } else if idx == setup::REVIEW_ROWS {
                        *selected = setup::REVIEW_ROWS - 1;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_changes_tab_key(&mut self, key: KeyEvent) {
        let ChangesTab {
            files,
            marked,
            rows,
            selected,
            ..
        } = &mut self.changes;
        // Scroll the diff content. Shift+Up/Down works on terminals that report
        // the modifier; Shift+J/Shift+K (which arrive as capital 'J'/'K' on any
        // terminal) are the always-available fallback. Plain Up/Down still move
        // the row cursor, so the scroll cases are handled first.
        let shift_arrow_down =
            key.code == KeyCode::Down && key.modifiers.contains(KeyModifiers::SHIFT);
        let shift_arrow_up = key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::SHIFT);
        if shift_arrow_down || key.code == KeyCode::Char('J') {
            self.scroll_diff(|s| s.saturating_add(3));
            return;
        }
        if shift_arrow_up || key.code == KeyCode::Char('K') {
            self.scroll_diff(|s| s.saturating_sub(3));
            return;
        }
        // Horizontal scroll: Shift+←/→ or H/L (plain h/l still tree-navigate).
        let shift_right = key.code == KeyCode::Right && key.modifiers.contains(KeyModifiers::SHIFT);
        let shift_left = key.code == KeyCode::Left && key.modifiers.contains(KeyModifiers::SHIFT);
        if shift_right || key.code == KeyCode::Char('L') {
            self.scroll_diff_h(|s| s.saturating_add(8));
            return;
        }
        if shift_left || key.code == KeyCode::Char('H') {
            self.scroll_diff_h(|s| s.saturating_sub(8));
            return;
        }
        match key.code {
            // Enter from the Worktrees list drills into Changes; q/Esc return
            // there (same idea as three-panel file focus). Quit only from the
            // Worktrees list, after the ignore window from this back expires.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.tab = Tab::Worktrees;
                self.worktrees_focus = WorktreesFocus::List;
                self.arm_ignore_quit();
            }
            KeyCode::Char('r') => self.refresh_diff(),
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < rows.len() {
                    *selected += 1;
                    self.load_diff_content(true);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if *selected > 0 {
                    *selected -= 1;
                    self.load_diff_content(true);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll_diff(|_| 0);
                self.scroll_diff_h(|_| 0);
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.tree_nav(key.code)
            }
            // Enter toggles a folder and opens a file with the OS default
            // application, the same as double-clicking the row.
            KeyCode::Enter => self.activate_changes_row(),
            KeyCode::Char(' ') => match rows.get(*selected) {
                // On a file row, toggle just that file.
                Some(DiffRow::File { index, .. }) => {
                    if let Some(m) = marked.get_mut(*index) {
                        *m = !*m;
                    }
                }
                // On a folder row, toggle every file under it together: if all
                // are on, turn them off, otherwise turn them all on.
                Some(DiffRow::Folder { prefix, .. }) => {
                    let prefix = prefix.clone();
                    let under: Vec<usize> = files
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| f.path.starts_with(&prefix))
                        .map(|(i, _)| i)
                        .collect();
                    let all_on = under
                        .iter()
                        .all(|&i| marked.get(i).copied().unwrap_or(false));
                    for i in under {
                        if let Some(m) = marked.get_mut(i) {
                            *m = !all_on;
                        }
                    }
                }
                None => {}
            },
            KeyCode::Char('a') => {
                let all_on = marked.iter().all(|m| *m);
                marked.iter_mut().for_each(|m| *m = !all_on);
            }
            KeyCode::Char('s') => {
                if let Some(e) =
                    current_file_index(rows, *selected).and_then(|i| files.get(i).cloned())
                {
                    self.stash_file(e);
                }
            }
            KeyCode::Char('S') => self.stash_marked(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.start_push(),
            // `u` undoes local changes to the file. `R` is reserved for rename
            // everywhere, so revert is not bound to it here.
            KeyCode::Char('u') => {
                let entry = current_file_index(rows, *selected).and_then(|i| files.get(i).cloned());
                match entry {
                    // A newly added file has no committed version to restore, so
                    // revert can't do anything; point the user at delete instead.
                    Some(e) if is_new_file(&e.code) => {
                        self.message = Some(format!(
                            "'{}' is new (not yet committed); nothing to revert to. Press d to delete it.",
                            e.path
                        ));
                    }
                    Some(e) => self.open_revert_modal(e.path),
                    None => {}
                }
            }
            KeyCode::Char('d') => {
                let path = current_file_index(rows, *selected)
                    .and_then(|i| files.get(i).map(|f| f.path.clone()));
                if let Some(path) = path {
                    self.open_delete_file_modal(path);
                }
            }
            KeyCode::Char('U') => {
                if !files.is_empty() {
                    self.open_discard_all_modal();
                }
            }
            KeyCode::Char('i') => {
                let target = match rows.get(*selected) {
                    Some(DiffRow::File { index, .. }) => files
                        .get(*index)
                        .map(|entry| (entry.path.clone(), ops::ignore_pattern(&entry.path), false)),
                    Some(DiffRow::Folder { prefix, label, .. }) => {
                        Some((prefix.clone(), format!("{label}/"), true))
                    }
                    None => None,
                };
                if let Some((file, pattern, is_folder)) = target {
                    self.open_ignore_modal(file, pattern, is_folder);
                }
            }
            KeyCode::Char('t') => self.toggle_file_layout(),
            KeyCode::Char('c') => self.commit_from_diff(),
            _ => {}
        }
    }

    /// Flips the changed-file list between the folder tree and a flat path list,
    /// rebuilding the rows and keeping the cursor on the same file when possible.
    fn toggle_file_layout(&mut self) {
        self.file_tree = !self.file_tree;
        let tree = self.file_tree;
        let ChangesTab {
            files,
            rows,
            selected,
            ..
        } = &mut self.changes;
        // Remember the file under the cursor so the toggle doesn't jump.
        let path = current_file_index(rows, *selected).map(|i| files[i].path.clone());
        *rows = build_rows(files, tree, &self.collapsed_folders);
        *selected = path
            .and_then(|p| {
                rows.iter().position(
                    |r| matches!(r, DiffRow::File { index, .. } if files[*index].path == p),
                )
            })
            .unwrap_or(0);
        self.load_diff_content(true);
    }

    /// Tree navigation for the changed-file browsers (changes view and commit
    /// browser): ← collapses the folder under the cursor (or jumps to the
    /// parent from a file), → expands a collapsed folder (or steps into an
    /// open one), and Enter toggles a folder. No-op in the flat layout.
    fn tree_nav(&mut self, code: KeyCode) {
        if !self.file_tree {
            return;
        }
        let is_browser = matches!(self.view, View::CommitDiff { .. } | View::StashDiff { .. });
        let (files, rows, selected) = match &mut self.view {
            View::CommitDiff {
                files,
                rows,
                selected,
                ..
            }
            | View::StashDiff {
                files,
                rows,
                selected,
                ..
            } => (files, rows, selected),
            // Otherwise this is the Changes tab (the only other caller).
            View::List if self.tab == Tab::Changes => {
                let c = &mut self.changes;
                (&mut c.files, &mut c.rows, &mut c.selected)
            }
            _ => return,
        };
        // What the key means on the row under the cursor.
        enum Nav {
            Collapse(String),
            Expand(String),
            Parent,
            Into,
        }
        let nav = match (code, rows.get(*selected)) {
            (
                KeyCode::Left | KeyCode::Char('h'),
                Some(DiffRow::Folder {
                    prefix,
                    collapsed: false,
                    ..
                }),
            ) => Nav::Collapse(prefix.clone()),
            (KeyCode::Left | KeyCode::Char('h'), Some(_)) => Nav::Parent,
            (
                KeyCode::Right | KeyCode::Char('l'),
                Some(DiffRow::Folder {
                    prefix,
                    collapsed: true,
                    ..
                }),
            ) => Nav::Expand(prefix.clone()),
            (
                KeyCode::Right | KeyCode::Char('l'),
                Some(DiffRow::Folder {
                    collapsed: false, ..
                }),
            ) => Nav::Into,
            (
                KeyCode::Enter,
                Some(DiffRow::Folder {
                    prefix, collapsed, ..
                }),
            ) => {
                if *collapsed {
                    Nav::Expand(prefix.clone())
                } else {
                    Nav::Collapse(prefix.clone())
                }
            }
            _ => return,
        };
        match nav {
            Nav::Collapse(prefix) | Nav::Expand(prefix) => {
                if !self.collapsed_folders.remove(&prefix) {
                    self.collapsed_folders.insert(prefix.clone());
                }
                *rows = build_diff_rows(files, &self.collapsed_folders);
                // Keep the cursor on the folder that was toggled.
                *selected = rows
                    .iter()
                    .position(|r| matches!(r, DiffRow::Folder { prefix: p, .. } if *p == prefix))
                    .unwrap_or(0);
            }
            // ← on a file or collapsed folder: jump to the nearest folder row
            // above with a smaller depth, i.e. the parent.
            Nav::Parent => {
                let depth = match rows.get(*selected) {
                    Some(DiffRow::Folder { depth, .. } | DiffRow::File { depth, .. }) => *depth,
                    None => return,
                };
                let Some(parent) = rows[..*selected]
                    .iter()
                    .rposition(|r| matches!(r, DiffRow::Folder { depth: d, .. } if *d < depth))
                else {
                    return;
                };
                *selected = parent;
            }
            // → on an open folder: step onto its first child.
            Nav::Into => {
                if *selected + 1 >= rows.len() {
                    return;
                }
                *selected += 1;
            }
        }
        if is_browser {
            self.load_file_browser_diff_content(true);
        } else {
            self.load_diff_content(true);
        }
    }

    /// Adds `pattern` to the worktree's `.gitignore`, then reloads the view.
    fn add_ignore(&mut self, pattern: &str) {
        let name = self.changes.name.clone();
        match ops::add_to_gitignore(&self.ctx, &name, pattern) {
            Ok(true) => self.message = Some(format!("added '{pattern}' to .gitignore")),
            Ok(false) => self.message = Some(format!("'{pattern}' is already in .gitignore")),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Applies `f` to the Changes tab's diff scroll offset.
    fn scroll_diff(&mut self, f: impl FnOnce(u16) -> u16) {
        self.changes.scroll = f(self.changes.scroll);
    }

    /// Applies `f` to the Changes tab's horizontal diff scroll, clamped to the
    /// longest line in the current content.
    fn scroll_diff_h(&mut self, f: impl FnOnce(u16) -> u16) {
        let max = self
            .changes
            .content
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0) as u16;
        self.changes.h_scroll = f(self.changes.h_scroll).min(max);
    }

    /// Rebuilds the changed-file list and the selected file's diff in place,
    /// preserving commit marks by path and clamping the cursor. No-op until the
    /// Changes tab has been opened on a worktree. Status runs off the UI thread
    /// so the 1s auto-refresh and `r` never hitch navigation.
    fn refresh_diff(&mut self) {
        if self.changes.name.is_empty() {
            return;
        }
        let name = self.changes.name.clone();
        // A name already gone from `worktrees` is a stale Changes target —
        // realign quietly before spawning any git work.
        if !self.worktrees.iter().any(|w| w.name == name) {
            self.realign_changes_after_list_reload();
            return;
        }
        self.start_status_refresh(name, true);
    }

    /// Starts a background `ops::status` for `name`. When `preserve_marks` is
    /// true (periodic refresh / `r`), commit marks and cursor survive; when
    /// false (switching worktree), the pane is reset to all-marked.
    /// Throttles via `last_refresh` and skips when a refresh for the same name
    /// is already in flight.
    fn start_status_refresh(&mut self, name: String, preserve_marks: bool) {
        // Stamp the throttle now so tick doesn't re-kick every frame while
        // status is in flight. Bump the generation so any older in-flight
        // result is ignored when it lands.
        self.changes.last_refresh = Instant::now();
        self.status_refresh_gen = self.status_refresh_gen.wrapping_add(1);
        let token = self.status_refresh_gen;
        if !preserve_marks {
            // Switching worktree: clear the previous pane immediately so we
            // never show another worktree's files under the new name.
            self.changes = ChangesTab {
                name: name.clone(),
                last_refresh: Instant::now(),
                ..ChangesTab::default()
            };
            self.worktree_commits = None;
            self.commits_pending = None;
        } else if self.changes.name != name {
            self.changes.name = name.clone();
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let name_for_thread = name.clone();
        std::thread::spawn(move || {
            let result = ops::status(&ctx, &name_for_thread)
                .map(|(_, files)| files)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.status_refresh_pending = Some((token, name, preserve_marks, Task::new(rx)));
    }

    /// Applies a finished background status refresh when its generation still
    /// matches. Called each tick.
    fn poll_status_refresh(&mut self) {
        let Some((token, name, preserve, task)) = &self.status_refresh_pending else {
            return;
        };
        let token = *token;
        let name = name.clone();
        let preserve = *preserve;
        let Some(result) = task.poll_latest() else {
            return;
        };
        self.status_refresh_pending = None;
        if token != self.status_refresh_gen || self.changes.name != name {
            return;
        }
        match result {
            Ok(files) => self.apply_changes_files(name, files, preserve),
            Err(e) => {
                if !self.worktrees.iter().any(|w| w.name == name) {
                    self.realign_changes_after_list_reload();
                    return;
                }
                self.set_error(e);
                self.changes = ChangesTab::default();
                self.worktree_commits = None;
                self.commits_pending = None;
                self.tab = Tab::Worktrees;
                self.refresh();
            }
        }
    }

    /// Installs `files` into the changes pane for `name`. When `preserve_marks`
    /// is true, keeps marks/cursor/scroll semantics of `refresh_diff`; otherwise
    /// resets like a fresh `load_changes`.
    fn apply_changes_files(&mut self, name: String, files: Vec<StatusEntry>, preserve_marks: bool) {
        let tree = self.file_tree;
        if preserve_marks {
            let old_path = {
                let c = &self.changes;
                current_file_index(&c.rows, c.selected)
                    .and_then(|i| c.files.get(i))
                    .map(|f| f.path.clone())
            };
            let c = &mut self.changes;
            let old: std::collections::HashMap<&str, bool> = c
                .files
                .iter()
                .zip(c.marked.iter())
                .map(|(f, m)| (f.path.as_str(), *m))
                .collect();
            let new_marked = files
                .iter()
                .map(|f| old.get(f.path.as_str()).copied().unwrap_or(true))
                .collect();
            c.rows = build_rows(&files, tree, &self.collapsed_folders);
            c.files = files;
            c.marked = new_marked;
            c.selected = c.selected.min(c.rows.len().saturating_sub(1));
            c.name = name;
            c.last_refresh = Instant::now();
            let new_path = current_file_index(&c.rows, c.selected)
                .and_then(|i| c.files.get(i))
                .map(|f| f.path.clone());
            self.load_diff_content(new_path != old_path);
        } else {
            let marked = vec![true; files.len()];
            let rows = build_rows(&files, tree, &self.collapsed_folders);
            self.changes = ChangesTab {
                name,
                files,
                marked,
                rows,
                last_refresh: Instant::now(),
                ..ChangesTab::default()
            };
            self.load_diff_content(true);
        }
        self.sync_worktree_commits_panel();
    }

    /// Stashes a single file from the Changes tab, then reloads it.
    fn stash_file(&mut self, entry: StatusEntry) {
        let name = self.changes.name.clone();
        match ops::stash_push_paths(&self.ctx, &name, std::slice::from_ref(&entry.path), None) {
            Ok(_) => self.message = Some(format!("stashed '{}'", entry.path)),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Stashes every marked (`[x]`) file from the Changes tab, then reloads it.
    /// Reports when nothing is marked rather than stashing the whole worktree.
    fn stash_marked(&mut self) {
        let name = self.changes.name.clone();
        let paths: Vec<String> = self
            .changes
            .files
            .iter()
            .zip(self.changes.marked.iter())
            .filter(|(_, m)| **m)
            .map(|(f, _)| f.path.clone())
            .collect();
        if paths.is_empty() {
            self.message = Some("no files marked; press Space to mark files first".to_string());
            return;
        }
        match ops::stash_push_paths(&self.ctx, &name, &paths, None) {
            Ok(_) => self.message = Some(format!("stashed {} marked file(s)", paths.len())),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Reverts a single file from the Changes tab, then reloads it.
    fn revert_file(&mut self, entry: StatusEntry) {
        let name = self.changes.name.clone();
        let untracked = entry.code.starts_with('?');
        match ops::revert_file(&self.ctx, &name, &entry.path, untracked) {
            Ok(_) => self.message = Some(format!("reverted '{}'", entry.path)),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Discards every uncommitted change in the Changes tab's worktree, then
    /// reloads it.
    fn discard_all_changes(&mut self) {
        let name = self.changes.name.clone();
        match ops::discard_all_changes(&self.ctx, &name) {
            Ok(_) => self.message = Some(format!("discarded all changes in '{name}'")),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Deletes a single file from the Changes tab, then reloads it.
    fn delete_file(&mut self, entry: StatusEntry) {
        let name = self.changes.name.clone();
        let untracked = entry.code.starts_with('?');
        match ops::delete_file(&self.ctx, &name, &entry.path, untracked) {
            Ok(_) => self.message = Some(format!("deleted '{}'", entry.path)),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Opens the commit dialog from the Changes tab, carrying the files marked
    /// there as the initial selection.
    fn commit_from_diff(&mut self) {
        let c = &self.changes;
        if c.files.is_empty() {
            self.message = Some("nothing to commit".to_string());
            return;
        }
        self.push_screen(View::Commit {
            name: c.name.clone(),
            files: c.files.clone(),
            marked: c.marked.clone(),
            cursor: 0,
            input: TextInput::default(),
            body: TextArea::default(),
            focus: CommitFocus::Message,
        });
    }

    /// Opens the new-worktree dialog. Row 0 creates a new branch off a base
    /// branch; the rows below check out an existing branch that isn't already
    /// in a worktree. The base defaults to the repo's main branch.
    fn open_create(&mut self) {
        let checked_out: Vec<&str> = self
            .worktrees
            .iter()
            .filter_map(|w| w.branch.as_deref())
            .collect();
        let all_branches = match crate::git::local_branches(&self.ctx.repo_root) {
            Ok(all) => all,
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        // Local branches not already in a worktree come first, then remote-only
        // branches (a teammate's work with no local copy) so they are
        // discoverable and can be checked out into a tracking branch. Remotes
        // are best-effort: a repo without them just yields the local list.
        let mut branches: Vec<CheckoutCandidate> = all_branches
            .iter()
            .filter(|b| !checked_out.contains(&b.as_str()))
            .map(|b| CheckoutCandidate {
                branch: b.clone(),
                remote: None,
            })
            .collect();
        if let Ok(remotes) = crate::git::remote_branches(&self.ctx.repo_root) {
            let mut seen: Vec<String> = all_branches.clone();
            for (short, remote_ref) in remotes {
                if seen.contains(&short) {
                    continue;
                }
                seen.push(short.clone());
                branches.push(CheckoutCandidate {
                    branch: short,
                    remote: Some(remote_ref),
                });
            }
        }
        let base = self.default_base(&all_branches);
        self.push_screen(View::Create {
            name: TextInput::default(),
            branches,
            all_branches,
            base,
            selected: 0,
            base_focus: false,
            base_pick: None,
        });
    }

    /// The base branch a new branch should default to: the main worktree's
    /// branch when it is a known local branch, else the first local branch,
    /// else `HEAD`.
    fn default_base(&self, all_branches: &[String]) -> String {
        self.worktrees
            .iter()
            .find(|w| w.is_main)
            .and_then(|w| w.branch.clone())
            .filter(|b| all_branches.iter().any(|x| x == b))
            .or_else(|| all_branches.first().cloned())
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// Drives the new-worktree dialog: edit the new-branch name, move over the
    /// checkout list, or pick the base branch to branch off of.
    fn on_create_key(&mut self, key: KeyEvent) {
        let View::Create {
            name,
            branches,
            all_branches,
            base,
            selected,
            base_focus,
            base_pick,
        } = &mut self.view
        else {
            return;
        };
        // Base-branch picker: a small overlay list of every local branch.
        if let Some(idx) = base_pick {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => *idx = idx.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if *idx + 1 < all_branches.len() {
                        *idx += 1;
                    }
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if let Some(b) = all_branches.get(*idx) {
                        *base = b.clone();
                    }
                    *base_pick = None;
                }
                KeyCode::Esc => *base_pick = None,
                _ => {}
            }
            return;
        }
        // Opens the base picker starting on the currently selected base.
        let open_base_pick =
            |base: &str, all_branches: &[String], base_pick: &mut Option<usize>| {
                let start = all_branches.iter().position(|b| b == base).unwrap_or(0);
                *base_pick = Some(start);
            };
        match key.code {
            // Esc backs out of the focused base button first, then the dialog.
            KeyCode::Esc => {
                if *base_focus {
                    *base_focus = false;
                } else {
                    self.navigate_back();
                }
            }
            // Tab focuses the base button on the new-branch row; a second Tab (or
            // Enter/Space while focused) opens the picker.
            KeyCode::Tab if *selected == 0 && !all_branches.is_empty() => {
                if *base_focus {
                    open_base_pick(base, all_branches, base_pick);
                } else {
                    *base_focus = true;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if *base_focus => {
                open_base_pick(base, all_branches, base_pick);
            }
            KeyCode::Down => {
                *base_focus = false;
                // Navigation is over the filtered checkout list, not `branches`.
                let filtered = filtered_candidates(branches, name.as_str());
                if *selected < filtered.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up => {
                *base_focus = false;
                *selected = selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                if *selected == 0 {
                    let branch = name.trimmed();
                    let base = base.clone();
                    if branch.is_empty() {
                        self.message = Some("type a name for the new branch".to_string());
                        return;
                    }
                    self.request_create(branch, Some(base));
                } else {
                    // Map the filtered cursor back to the real candidate. A
                    // remote-only branch is created as a local tracking branch
                    // off its remote ref; a local branch is checked out directly.
                    let filtered = filtered_candidates(branches, name.as_str());
                    let Some(&idx) = filtered.get(*selected - 1) else {
                        return;
                    };
                    let candidate = branches[idx].clone();
                    self.request_create(candidate.branch, candidate.remote);
                }
            }
            // Any other key returns focus to the new-branch name and edits it.
            _ => {
                *base_focus = false;
                if name.on_key(key) {
                    *selected = 0;
                }
            }
        }
    }

    /// Starts a create for `branch` (new branch when `base` is `Some`). First
    /// checks whether the branch it would start from (`base`, or `branch`
    /// itself when checking out an existing local branch) is behind its
    /// upstream and, if so, offers to pull it first.
    fn request_create(&mut self, branch: String, base: Option<String>) {
        let target = base.clone().unwrap_or_else(|| branch.clone());
        match ops::branch_ahead_behind(&self.ctx, &target) {
            Ok(Some(ab)) if ab.behind > 0 && ab.ahead == 0 => {
                self.open_confirm_pull_base_modal(branch, base, target, ab.behind);
            }
            _ => self.request_create_checked(branch, base),
        }
    }

    /// Confirmation for pulling `target` (behind its upstream by `behind`
    /// commits) before starting the create.
    fn open_confirm_pull_base_modal(
        &mut self,
        branch: String,
        base: Option<String>,
        target: String,
        behind: u32,
    ) {
        let commits = if behind == 1 { "commit" } else { "commits" };
        let body = vec![Line::from(format!(
            "'{target}' is {behind} {commits} behind its upstream. pull it first?"
        ))];
        let options = vec![
            ConfirmOption::new("pull, then create"),
            ConfirmOption::new("create without pulling"),
        ];
        self.push_confirm(
            "branch is behind",
            body,
            options,
            ModalAction::ConfirmPullBase {
                branch,
                base,
                target,
            },
        );
    }

    /// Continues a create for `branch` (new branch when `base` is `Some`)
    /// after the behind-upstream check: checks whether the target directory
    /// already exists and, if so, asks the user what to do about it.
    fn request_create_checked(&mut self, branch: String, base: Option<String>) {
        match ops::existing_target(&self.ctx, &branch) {
            Ok(Some(target)) => {
                let path = target.path.to_string_lossy().to_string();
                self.open_confirm_existing_modal(branch, base, path, target.worktree_name);
            }
            Ok(None) => self.start_create(branch, base),
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Force-removes the directory at `path` (even when non-empty) and, on
    /// success, starts creating the worktree for `branch` in its place.
    fn replace_target(&mut self, branch: String, base: Option<String>, path: &str) {
        match ops::remove_target(&self.ctx, Path::new(path)) {
            Ok(()) => self.start_create(branch, base),
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Kicks off `ops::create` on a background thread so setup commands
    /// (npm install etc.) don't freeze the UI. `base` is the ref a new branch
    /// is created from; `None` checks out an existing branch.
    fn start_create(&mut self, branch: String, base: Option<String>) {
        let (tx, rx) = channel();
        let control = SetupControl::default();
        let ctx = self.ctx.clone();
        let thread_branch = branch.clone();
        let thread_control = control.clone();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = ops::create(
                &ctx,
                &thread_branch,
                base.as_deref(),
                ops::RunMode::Controlled(thread_control),
                move |line| {
                    let _ = progress_tx.send(CreateMsg::Progress(line.to_string()));
                },
            );
            let _ = tx.send(CreateMsg::Done(result.map_err(|e| format!("{e:#}"))));
        });
        self.view = View::Creating {
            branch,
            lines: Vec::new(),
            rx: Task::new(rx),
            done: false,
            outcome: None,
            control,
            input: String::new(),
            kill_armed: false,
        };
    }

    /// Runs a configured `open_command` for the selected worktree. Any
    /// configured commands open a picker (even a single one, so the command is
    /// always confirmed before it runs); none opens a one-off prompt.
    /// Templates may use `{path}`, `{name}`, `{branch}`, and `{status}`.
    fn run_open_command(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        let path = wt.path.clone();
        let branch = wt.branch.clone().unwrap_or_default();
        let status = wt.open_status().to_string();
        let commands: Vec<OpenCommand> = self
            .ctx
            .config
            .open_command
            .iter()
            .filter(|c| !c.command.trim().is_empty())
            .cloned()
            .collect();
        match commands.as_slice() {
            [] => self.push_screen(View::RunCommand {
                name,
                path,
                input: TextInput::default(),
            }),
            _ => self.push_screen(View::OpenCommand {
                name,
                path,
                branch,
                status,
                commands,
                selected: 0,
            }),
        }
    }

    /// Key handling for the open-command picker.
    fn on_open_command_key(&mut self, key: KeyEvent) {
        let View::OpenCommand {
            commands, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < commands.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.confirm_open_command(),
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            _ => {}
        }
    }

    /// Runs the open-command currently highlighted in the picker.
    fn confirm_open_command(&mut self) {
        let View::OpenCommand {
            name,
            path,
            branch,
            status,
            commands,
            selected,
        } = &self.view
        else {
            return;
        };
        let Some(cmd) = commands.get(*selected).cloned() else {
            return;
        };
        let (name, path, branch, status) =
            (name.clone(), path.clone(), branch.clone(), status.clone());
        self.pop_screen();
        self.spawn_open_command(&cmd, &name, &path, &branch, &status);
    }

    /// Expands placeholders in `command` and runs it according to its mode:
    /// background commands are spawned detached and leave the TUI up, terminal
    /// commands quit the TUI so `tui::run` can hand this terminal over once it
    /// is restored.
    fn spawn_open_command(
        &mut self,
        command: &OpenCommand,
        name: &str,
        path: &str,
        branch: &str,
        status: &str,
    ) {
        let cmd = crate::config::expand_open_command(
            &command.command,
            &crate::config::OpenCommandVars {
                path,
                name,
                branch,
                status,
            },
        );
        match command.mode {
            CommandMode::Background => self.spawn_in_dir(&cmd, path, name),
            CommandMode::Terminal => {
                self.exec_on_exit = Some((cmd, path.to_string()));
                self.quit = true;
            }
        }
    }

    /// Spawns `cmd` through the shell, detached, in `dir`. Stdio is detached so
    /// GUI tools like `cursor .` open without disturbing the TUI. Intended for
    /// background/GUI commands, not terminal programs that need this terminal.
    fn spawn_in_dir(&mut self, cmd: &str, dir: &str, name: &str) {
        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match result {
            Ok(_) => self.message = Some(format!("ran '{cmd}' in '{name}'")),
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Opens the commit flow for the selected worktree, or reports it clean.
    fn open_commit(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        if wt.dirty == 0 {
            self.message = Some(format!(
                "worktree '{}' is clean, nothing to commit",
                wt.name
            ));
            return;
        }
        let name = wt.name.clone();
        match ops::status(&self.ctx, &name) {
            Ok((_, files)) => {
                let marked = vec![true; files.len()];
                self.push_screen(View::Commit {
                    name,
                    files,
                    marked,
                    cursor: 0,
                    input: TextInput::default(),
                    body: TextArea::default(),
                    focus: CommitFocus::Message,
                });
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Drives the commit dialog. The file list and message input each own a
    /// focus; Tab switches between them and Enter commits the marked files.
    fn on_commit_key(&mut self, key: KeyEvent) {
        let View::Commit {
            files,
            marked,
            cursor,
            input,
            body,
            focus,
            ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.navigate_back();
                return;
            }
            KeyCode::Tab => {
                *focus = match focus {
                    CommitFocus::Files => CommitFocus::Message,
                    CommitFocus::Message => CommitFocus::Body,
                    CommitFocus::Body => CommitFocus::Files,
                };
                return;
            }
            KeyCode::BackTab => {
                *focus = match focus {
                    CommitFocus::Files => CommitFocus::Body,
                    CommitFocus::Message => CommitFocus::Files,
                    CommitFocus::Body => CommitFocus::Message,
                };
                return;
            }
            // Enter inserts a newline in the body, so committing from there
            // needs its own key; it works from the other panes too.
            KeyCode::Char('s') | KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.do_commit();
                return;
            }
            // Enter still commits from the file list and the subject line.
            KeyCode::Enter if *focus != CommitFocus::Body => {
                self.do_commit();
                return;
            }
            _ => {}
        }
        match focus {
            CommitFocus::Files => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if *cursor + 1 < files.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Char(' ') => {
                    if let Some(m) = marked.get_mut(*cursor) {
                        *m = !*m;
                    }
                }
                KeyCode::Char('a') => {
                    let all_on = marked.iter().all(|m| *m);
                    marked.iter_mut().for_each(|m| *m = !all_on);
                }
                _ => {}
            },
            CommitFocus::Message => {
                input.on_key(key);
            }
            CommitFocus::Body => body.on_key(key),
        }
    }

    /// Commits the files marked in the commit dialog. Errors and empty
    /// selections keep the dialog open.
    fn do_commit(&mut self) {
        let View::Commit {
            name,
            files,
            marked,
            input,
            body,
            ..
        } = &self.view
        else {
            return;
        };
        let body = body.text().trim().to_string();
        let body = (!body.is_empty()).then_some(body);
        let message = input.trimmed();
        if message.is_empty() {
            self.message = Some("commit message must not be empty".to_string());
            return;
        }
        let paths: Vec<String> = files
            .iter()
            .zip(marked.iter())
            .filter(|(_, m)| **m)
            .map(|(f, _)| f.path.clone())
            .collect();
        if paths.is_empty() {
            self.message = Some("select at least one file to commit".to_string());
            return;
        }
        let name = name.clone();
        self.start_busy(
            format!("committing '{name}'…"),
            BusyThen::List,
            move |ctx| {
                ops::commit(ctx, &name, &message, body.as_deref(), Some(&paths))
                    .map(|r| {
                        format!(
                            "committed {} · {} ({} file{})",
                            r.hash,
                            r.summary,
                            r.files_changed,
                            if r.files_changed == 1 { "" } else { "s" }
                        )
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Switches to the Settings tab with the repo's `.wtm.toml` freshly read,
    /// so it never shows values that have gone stale on disk.
    fn open_settings_tab(&mut self) {
        match self.settings.reload() {
            Ok(()) => self.tab = Tab::Settings,
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    fn on_settings_tab_key(&mut self, key: KeyEvent) {
        match self.settings.on_key(key, &mut self.message) {
            EditorOutcome::Saved(path) => {
                self.reload_config();
                if self.message.is_none() {
                    self.message = Some(format!("saved {}", path.display()));
                }
            }
            EditorOutcome::CheckForUpdates => {
                self.start_update_check(true);
                self.message = Some("checking for updates…".to_string());
            }
            // The editor swallows Esc itself while a field is being edited, so
            // `Cancel` only arrives when nothing is in progress — and then Esc/q
            // quits, as on every other tab.
            EditorOutcome::Cancel => self.request_quit(),
            EditorOutcome::Continue => {}
        }
    }

    /// Switches to the Stash tab, loaded with the selected worktree's stashes.
    fn open_stash_tab(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        // A different worktree than last time starts at the top of its list.
        if name != self.stash_name {
            self.stash_selected = 0;
        }
        self.tab = Tab::Stash;
        self.reload_stash_tab(name);
    }

    /// Re-reads the stash list for `name` into the tab on a background thread,
    /// keeping the cursor on a valid row once the result lands. Used on tab
    /// entry and after a background stash op.
    fn reload_stash_tab(&mut self, name: String) {
        // A different worktree than last time starts at the top and clears the
        // old list so we never show another worktree's stashes under the new name.
        if name != self.stash_name {
            self.stash_selected = 0;
            self.stash_entries.clear();
        }
        self.stash_name = name.clone();
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let name_for_thread = name.clone();
        std::thread::spawn(move || {
            let result = ops::stash_list(&ctx, &name_for_thread)
                .map(|r| r.entries)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.stash_pending = Some((name, Task::new(rx)));
    }

    /// Applies a finished background stash-list load when it still matches the
    /// Stash tab's worktree. Called each tick.
    fn poll_stash_load(&mut self) {
        let Some((name, task)) = &self.stash_pending else {
            return;
        };
        let name = name.clone();
        let Some(result) = task.poll_latest() else {
            return;
        };
        self.stash_pending = None;
        if name != self.stash_name {
            return;
        }
        match result {
            Ok(entries) => {
                self.stash_entries = entries;
                self.stash_selected = self
                    .stash_selected
                    .min(self.stash_entries.len().saturating_sub(1));
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Whether the Stash tab is waiting on a background list load.
    #[cfg(test)]
    fn stash_loading(&self) -> bool {
        self.stash_pending.is_some()
    }

    fn on_stash_tab_key(&mut self, key: KeyEvent) {
        let index = self.stash_entries.get(self.stash_selected).map(|e| e.index);
        let name = self.stash_name.clone();
        match key.code {
            // Every top-level tab quits on Esc/q; only drill-ins go "back".
            KeyCode::Esc | KeyCode::Char('q') => self.request_quit(),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.stash_selected + 1 < self.stash_entries.len() {
                    self.stash_selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.stash_selected = self.stash_selected.saturating_sub(1)
            }
            KeyCode::Char('s') => self.push_prompt(
                "stash message (optional)",
                TextInput::default(),
                "blank Enter stashes without a message",
                ModalAction::StashPush { name },
            ),
            KeyCode::Char('p') => self.open_stash_target_pick(true),
            KeyCode::Char('a') => self.open_stash_target_pick(false),
            KeyCode::Char('x') => {
                if !self.stash_entries.is_empty() {
                    self.open_stash_drop_modal(name, index);
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = self.stash_entries.get(self.stash_selected) {
                    let label = format!("stash@{{{}}}: {}", entry.index, entry.message);
                    let index = entry.index;
                    let name = self.stash_name.clone();
                    self.open_stash_diff(name, index, label);
                }
            }
            _ => {}
        }
    }

    /// Opens the destination picker for applying/popping the selected stash
    /// entry. Stashes are repo-global, so any worktree is a valid target; the
    /// cursor defaults to the worktree the tab was opened from.
    fn open_stash_target_pick(&mut self, pop: bool) {
        let index = self.stash_entries.get(self.stash_selected).map(|e| e.index);
        let label = match index {
            Some(i) => format!("stash@{{{i}}}"),
            None => "the most recent stash".to_string(),
        };
        let targets: Vec<CherryTarget> = self
            .worktrees
            .iter()
            .map(|w| CherryTarget {
                name: w.name.clone(),
                branch: w.branch.clone(),
            })
            .collect();
        if targets.is_empty() {
            self.message = Some("no worktrees to apply into".to_string());
            return;
        }
        let selected = targets
            .iter()
            .position(|t| t.name == self.stash_name)
            .unwrap_or(0);
        self.push_screen(View::StashTarget {
            pop,
            index,
            label,
            targets,
            selected,
        });
    }

    /// Key handling for the stash target picker: pick a worktree, then apply
    /// or pop the stash into it.
    /// Opens the upstream picker for the branch under the cursor on the
    /// Branches tab. Refuses on a remote-only row (there is no local branch to
    /// give an upstream) and when the repo has no remote-tracking refs at all.
    fn open_upstream_pick(&mut self) {
        let Some(branch) = self.branches.get(self.branch_selected) else {
            return;
        };
        if branch.remote.is_some() {
            self.message = Some(format!(
                "'{}' only exists on a remote; check it out first to give it an upstream",
                branch.name
            ));
            return;
        }
        let (name, current) = (branch.name.clone(), branch.upstream.clone());
        let candidates = match ops::upstream_candidates(&self.ctx) {
            Ok(c) => c,
            Err(e) => return self.set_error(format!("{e:#}")),
        };
        if candidates.is_empty() && current.is_none() {
            self.message = Some("no remote branches to track · fetch first".to_string());
            return;
        }
        self.push_screen(View::UpstreamPick {
            branch: name,
            current,
            candidates,
            filter: TextInput::default(),
            selected: 0,
        });
    }

    /// Key handling for the upstream picker: typing narrows the list, Enter
    /// applies the highlighted row.
    fn on_upstream_pick_key(&mut self, key: KeyEvent) {
        let View::UpstreamPick {
            current,
            candidates,
            filter,
            selected,
            ..
        } = &mut self.view
        else {
            return;
        };
        let row_count = |filter: &TextInput, selected: usize| {
            let rows = upstream_rows(candidates, filter.as_str(), current.is_some());
            selected.min(rows.len().saturating_sub(1))
        };
        match key.code {
            KeyCode::Down => {
                let rows = upstream_rows(candidates, filter.as_str(), current.is_some());
                if *selected + 1 < rows.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.confirm_upstream_pick(),
            KeyCode::Esc => self.navigate_back(),
            _ => {
                // Every other key types into the filter, so a branch name with
                // a `j`/`k` in it narrows rather than moving the cursor.
                filter.on_key(key);
                *selected = row_count(filter, *selected);
            }
        }
    }

    /// Applies the highlighted upstream row to the branch and reloads the list.
    fn confirm_upstream_pick(&mut self) {
        let View::UpstreamPick {
            branch,
            current,
            candidates,
            filter,
            selected,
        } = &self.view
        else {
            return;
        };
        let rows = upstream_rows(candidates, filter.as_str(), current.is_some());
        let (branch, upstream) = match rows.get(*selected) {
            Some(UpstreamRow::Unset) => (branch.clone(), None),
            Some(UpstreamRow::Candidate(i)) => match candidates.get(*i) {
                Some(c) => (branch.clone(), Some(c.clone())),
                None => return,
            },
            None => return,
        };
        self.pop_screen();
        match ops::branch_set_upstream(&self.ctx, &branch, upstream.as_deref()) {
            Ok(result) => {
                self.message = Some(match &result.upstream {
                    Some(now) => format!("'{branch}' now tracks {now}"),
                    None => format!("'{branch}' no longer tracks a remote branch"),
                });
                // The row's tracking column and ahead/behind counts both change,
                // so the cached list is stale until it reloads.
                self.load_branches(self.branch_selected);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    fn on_stash_target_key(&mut self, key: KeyEvent) {
        let View::StashTarget {
            targets, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < targets.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.run_stash_target(),
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            _ => {}
        }
    }

    /// Applies or pops the picked stash entry into the chosen worktree.
    fn run_stash_target(&mut self) {
        let picked = match &self.view {
            View::StashTarget {
                pop,
                index,
                targets,
                selected,
                ..
            } => targets
                .get(*selected)
                .map(|t| (*pop, *index, t.name.clone())),
            _ => None,
        };
        let Some((pop, index, name)) = picked else {
            return;
        };
        if pop {
            self.stash_pop(name, index);
        } else {
            self.stash_action("apply", name, index);
        }
    }

    /// Runs an apply/drop on `name`, reports the result, and reloads the tab
    /// (dirty counts and the stash list may both have changed). Pop is handled
    /// separately by `stash_pop`, since it can leave conflicts to resolve.
    fn stash_action(&mut self, action: &str, name: String, index: Option<u32>) {
        let action = action.to_string();
        self.start_busy(
            format!("stash {action}…"),
            BusyThen::Stash(name.clone()),
            move |ctx| {
                let result = match action.as_str() {
                    "apply" => ops::stash_apply(ctx, &name, index),
                    _ => ops::stash_drop(ctx, &name, index),
                };
                result
                    .map(|r| format!("stash {} on '{}'", r.action, r.name))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Pops a stash on `name` in the background. A clean pop returns to the stash
    /// tab; a conflicting pop routes into the resolver (kind `StashPop`),
    /// which finishes by dropping the stash once every file is resolved.
    fn stash_pop(&mut self, name: String, index: Option<u32>) {
        let n = name.clone();
        self.start_busy(
            "stash pop…".to_string(),
            BusyThen::Resolve {
                target: name,
                source_label: "the stashed changes".to_string(),
                kind: ops::ResolveKind::StashPop { index },
            },
            move |ctx| {
                ops::stash_pop(ctx, &n, index)
                    .map(|outcome| match outcome {
                        ops::StashPopOutcome::Applied { name, .. } => {
                            format!("popped stash on '{name}'")
                        }
                        // The message is unused on conflict; the resolver opens.
                        ops::StashPopOutcome::Conflicted { .. } => {
                            "conflicts to resolve".to_string()
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Stashes the worktree's current changes with an optional message.
    fn stash_push(&mut self, name: String, message: Option<String>) {
        self.start_busy(
            "stashing…".to_string(),
            BusyThen::Stash(name.clone()),
            move |ctx| {
                ops::stash_push(ctx, &name, message.as_deref())
                    .map(|_| format!("stashed changes in '{name}'"))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Opens the switch-branch picker for the selected worktree: local branches
    /// not checked out in any worktree (so git will let us switch onto them),
    /// followed by remote-only branches.
    fn open_switch(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        // Every branch currently checked out somewhere (includes this worktree's
        // own current branch), which git forbids switching onto.
        let checked_out: Vec<String> = self
            .worktrees
            .iter()
            .filter_map(|w| w.branch.clone())
            .collect();
        let local = match crate::git::local_branches(&self.ctx.repo_root) {
            Ok(all) => all,
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        let mut branches: Vec<CheckoutCandidate> = local
            .iter()
            .filter(|b| !checked_out.contains(b))
            .map(|b| CheckoutCandidate {
                branch: b.clone(),
                remote: None,
            })
            .collect();
        // Remote-only branches (a teammate's work with no local copy) follow the
        // local ones, so switching onto one creates a local tracking branch.
        // Remotes are best-effort: a repo without them just yields the local list.
        if let Ok(remotes) = crate::git::remote_branches(&self.ctx.repo_root) {
            let mut seen = local;
            for (short, remote_ref) in remotes {
                if seen.contains(&short) {
                    continue;
                }
                seen.push(short.clone());
                branches.push(CheckoutCandidate {
                    branch: short,
                    remote: Some(remote_ref),
                });
            }
        }
        self.push_screen(View::Switch {
            name,
            branches,
            filter: TextInput::default(),
            selected: 0,
        });
    }

    /// Drives the switch-branch picker: type to filter the branch list, move
    /// the cursor within the filtered results, or switch on Enter (with no
    /// match, on the typed name itself). Esc clears an active filter first,
    /// then closes the view on a second press.
    fn on_switch_key(&mut self, key: KeyEvent) {
        let View::Switch {
            name,
            branches,
            filter,
            selected,
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                if !filter.as_str().is_empty() {
                    *filter = TextInput::default();
                    *selected = 0;
                } else {
                    self.navigate_back();
                }
            }
            KeyCode::Down => {
                let count = filtered_candidates(branches, filter.as_str()).len();
                if *selected + 1 < count {
                    *selected += 1;
                }
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Enter => {
                let filtered = filtered_candidates(branches, filter.as_str());
                // Picking a listed candidate switches onto it. With no match, the
                // typed name is created as a new local branch if it doesn't exist
                // anywhere (and otherwise switched onto, e.g. a branch added since
                // the list was built) — so typing a fresh name makes a branch.
                let choice = match filtered.get(*selected) {
                    Some(&idx) => Some((branches[idx].branch.clone(), false)),
                    None => {
                        let typed = filter.as_str().trim().to_string();
                        (!typed.is_empty()).then_some((typed, true))
                    }
                };
                if let Some((branch, create)) = choice {
                    let name = name.clone();
                    self.request_switch(name, branch, create);
                }
            }
            _ => {
                if filter.on_key(key) {
                    // The filtered set just changed; keep the cursor in bounds
                    // rather than pointing past the new (likely shorter) list.
                    let count = filtered_candidates(branches, filter.as_str()).len();
                    *selected = (*selected).min(count.saturating_sub(1));
                }
            }
        }
    }

    /// Switches the worktree named `name` onto `branch` in the background,
    /// creating `branch` as a new local branch off its HEAD when `create` is set
    /// and no such branch exists yet.
    fn request_switch(&mut self, name: String, branch: String, create: bool) {
        let verb = if create { "creating" } else { "switching to" };
        self.start_busy(
            format!("{verb} {branch} in {name}…"),
            BusyThen::List,
            move |ctx| {
                ops::switch_branch(ctx, &name, &branch, create)
                    .map(|r| format!("switched '{}' to '{}'", r.name, r.branch))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Starts a background `ops::status` for the highlighted worktree when the
    /// cached preview belongs to a different row (or was invalidated). Safe to
    /// call every frame: a load already in flight for `selected` is left alone,
    /// so the draw path never spawns unbounded threads.
    pub fn ensure_worktree_preview(&mut self) {
        if self.preview_for == Some(self.selected) {
            return;
        }
        if self
            .preview_pending
            .as_ref()
            .is_some_and(|(idx, _)| *idx == self.selected)
        {
            return;
        }
        self.load_worktree_preview(self.selected);
    }

    /// Spawns `ops::status` for `selected` on a background thread. The UI keeps
    /// drawing; `poll_preview_load` applies the files once they land.
    fn load_worktree_preview(&mut self, selected: usize) {
        let Some(name) = self.worktrees.get(selected).map(|w| w.name.clone()) else {
            self.worktree_preview.clear();
            self.preview_for = Some(selected);
            self.preview_pending = None;
            return;
        };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let files = ops::status(&ctx, &name)
                .map(|(_, files)| files)
                .unwrap_or_default();
            let _ = tx.send(files);
        });
        self.preview_pending = Some((selected, Task::new(rx)));
    }

    /// Applies a finished background preview load when it matches the current
    /// selection. Called each tick (and from tests) so status never blocks
    /// the render thread.
    fn poll_preview_load(&mut self) {
        let Some((idx, task)) = &self.preview_pending else {
            return;
        };
        let idx = *idx;
        let Some(files) = task.poll_latest() else {
            return;
        };
        self.preview_pending = None;
        // Selection moved while the thread ran; drop the stale list. The next
        // `ensure_worktree_preview` will fetch for the new row.
        if idx != self.selected {
            return;
        }
        self.worktree_preview = files;
        self.preview_for = Some(idx);
        self.sync_three_panel_changes();
    }

    /// Hands a freshly loaded worktree status to the three-panel layout's
    /// changed-file and diff panels, so moving the worktree cursor swaps them
    /// without an `ops::status` on the UI thread. The status itself comes from
    /// the preview loader, which already drops results for a selection the
    /// cursor has since moved off. A clean worktree swaps the bottom area to
    /// that branch's commit list instead of an empty changes pane.
    fn sync_three_panel_changes(&mut self) {
        if !self.three_panel {
            self.worktree_commits = None;
            return;
        }
        let Some(name) = self.worktrees.get(self.selected).map(|w| w.name.clone()) else {
            return;
        };
        // Already showing this worktree: leave the cursor, marks, and scroll
        // alone (the periodic `refresh_diff` keeps the contents current), but
        // still reconcile dirty ↔ commits when the panel was cleared.
        if self.changes.name != name {
            let files = self.worktree_preview.clone();
            let marked = vec![true; files.len()];
            let rows = build_rows(&files, self.file_tree, &self.collapsed_folders);
            self.changes = ChangesTab {
                name,
                files,
                marked,
                rows,
                ..ChangesTab::default()
            };
            self.load_diff_content(true);
        }
        self.sync_worktree_commits_panel();
    }

    /// Shows the branch commit list in the three-panel bottom area when the
    /// current changes pane is empty, or clears it when there are uncommitted
    /// files again. Reuses `commit_log_lines` (same path as `View::BranchCommits`).
    fn sync_worktree_commits_panel(&mut self) {
        if !self.three_panel || self.changes.name.is_empty() {
            self.worktree_commits = None;
            self.commits_pending = None;
            return;
        }
        if !self.changes.files.is_empty() {
            self.worktree_commits = None;
            self.commits_pending = None;
            return;
        }
        let Some(wt) = self.worktrees.iter().find(|w| w.name == self.changes.name) else {
            self.worktree_commits = None;
            self.commits_pending = None;
            return;
        };
        let name = wt.name.clone();
        let from_branch = wt.branch.is_some();
        let branch = wt.branch.clone().unwrap_or_else(|| name.clone());
        if let Some(panel) = &self.worktree_commits
            && panel.name == name
            && panel.branch == branch
            && panel.from_branch == from_branch
        {
            return;
        }
        self.load_worktree_commits_panel(name, branch, from_branch);
    }

    /// Loads commit rows for the three-panel clean-worktree panel off-thread so
    /// selection changes never block on `git log --graph`.
    fn load_worktree_commits_panel(&mut self, name: String, branch: String, from_branch: bool) {
        let key = CommitsLoadKey {
            name,
            branch,
            from_branch,
            log_mode: self.log_mode,
        };
        if self
            .commits_pending
            .as_ref()
            .is_some_and(|(k, _)| k == &key)
        {
            return;
        }
        // Drop a mismatched panel immediately so we never flash another
        // worktree's history while the new load runs.
        if self
            .worktree_commits
            .as_ref()
            .is_some_and(|p| p.name != key.name || p.branch != key.branch)
        {
            self.worktree_commits = None;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let name = key.name.clone();
        let branch = key.branch.clone();
        let from_branch = key.from_branch;
        let mode = key.log_mode;
        std::thread::spawn(move || {
            let _ = tx.send(commit_log_lines(&ctx, &name, &branch, from_branch, mode));
        });
        self.commits_pending = Some((key, Task::new(rx)));
    }

    /// Applies a finished three-panel commits load when it still matches the
    /// current clean worktree / log mode. Called each tick.
    fn poll_commits_load(&mut self) {
        let Some((key, task)) = &self.commits_pending else {
            return;
        };
        let key = key.clone();
        let Some(result) = task.poll_latest() else {
            return;
        };
        self.commits_pending = None;
        // Selection or mode moved while the thread ran.
        let current = CommitsLoadKey {
            name: self.changes.name.clone(),
            branch: self
                .worktrees
                .iter()
                .find(|w| w.name == self.changes.name)
                .and_then(|w| w.branch.clone())
                .unwrap_or_else(|| self.changes.name.clone()),
            from_branch: self
                .worktrees
                .iter()
                .find(|w| w.name == self.changes.name)
                .is_some_and(|w| w.branch.is_some()),
            log_mode: self.log_mode,
        };
        if key != current || !self.changes.files.is_empty() || !self.three_panel {
            return;
        }
        match result {
            Ok(lines) => {
                let selected = first_commit_row(&lines);
                self.worktree_commits = Some(WorktreeCommitsPanel {
                    name: key.name,
                    branch: key.branch,
                    from_branch: key.from_branch,
                    lines,
                    selected,
                });
            }
            Err(e) => {
                self.worktree_commits = None;
                self.message = Some(e);
            }
        }
    }

    /// Whether the three-panel commits panel is waiting on a background log.
    pub fn commits_loading(&self) -> bool {
        self.commits_pending.is_some()
            && (self.worktree_commits.is_none()
                || self.commits_pending.as_ref().is_some_and(|(k, _)| {
                    self.worktree_commits
                        .as_ref()
                        .is_some_and(|p| p.name != k.name || p.branch != k.branch)
                }))
    }

    /// Whether the three-panel bottom area is showing commits for a clean worktree.
    #[cfg(test)]
    fn showing_worktree_commits(&self) -> bool {
        self.three_panel && self.worktree_commits.is_some()
    }

    /// Whether the Worktrees tab preview is waiting on status for the current
    /// selection (no cached rows for that index yet). Used to draw "loading…"
    /// instead of another worktree's stale file list.
    pub fn preview_loading(&self) -> bool {
        self.preview_for != Some(self.selected)
            && self
                .preview_pending
                .as_ref()
                .is_some_and(|(idx, _)| *idx == self.selected)
    }

    /// Kicks off a (re)load of all local branches for the Branches tab on a
    /// background thread — building the list runs a git invocation per
    /// branch, so doing it on the UI thread would freeze the tab switch.
    /// `branches` is left as-is (stale) until the result lands in
    /// `poll_branches_load`, so re-entering the tab shows the previous list
    /// immediately instead of flashing empty.
    fn load_branches(&mut self, selected: usize) {
        self.branches_want_selected = selected;
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let result = ops::branch_list(&ctx).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.branches_pending = Some(Task::new(rx));
    }

    /// Loads the Branches tab only when the cache is missing, dirty, or older
    /// than `branches_refresh_mins`. A load already in flight is left alone.
    fn ensure_branches(&mut self, selected: usize) {
        if self.branches_pending.is_some() {
            self.branches_want_selected = selected;
            return;
        }
        let timeout =
            Duration::from_secs(self.ctx.config.branches_refresh_mins().saturating_mul(60));
        let fresh = self
            .branches_loaded_at
            .is_some_and(|at| at.elapsed() < timeout);
        if fresh && !self.branches.is_empty() {
            self.branch_selected = selected.min(self.branches.len().saturating_sub(1));
            return;
        }
        self.load_branches(selected);
    }

    /// Drops the Branches tab cache so the next `ensure_branches` reloads.
    /// Called after creating a worktree (and any other mutation that adds a
    /// branch without going through the Branches tab's own reload path).
    fn invalidate_branches_cache(&mut self) {
        self.branches_loaded_at = None;
    }

    /// Applies a finished background `load_branches` result, if one has
    /// landed. Called each tick so the branch list fills in (or refreshes)
    /// without blocking navigation. Bounces back to the Worktrees tab on
    /// error.
    fn poll_branches_load(&mut self) {
        let Some(task) = &self.branches_pending else {
            return;
        };
        let Some(result) = task.poll_latest() else {
            return;
        };
        self.branches_pending = None;
        match result {
            Ok(r) => {
                self.branch_selected = self
                    .branches_want_selected
                    .min(r.branches.len().saturating_sub(1));
                self.branches = r.branches;
                self.branches_loaded_at = Some(Instant::now());
            }
            Err(e) => {
                self.set_error(e);
                self.tab = Tab::Worktrees;
            }
        }
    }

    /// Whether the Branches tab is showing its very first load: no cached
    /// branches yet and a load is in flight. Used to draw a loading state
    /// instead of an empty list; a background refresh of an already-cached
    /// list doesn't trigger this.
    pub fn branches_first_load(&self) -> bool {
        self.branches_pending.is_some() && self.branches.is_empty()
    }

    /// Whether a `load_branches` reload is running in the background, cached
    /// list or not. Used to show a subtle "refreshing" hint alongside a
    /// still-visible stale list.
    pub fn branches_loading(&self) -> bool {
        self.branches_pending.is_some()
    }

    /// Selects the branch drawn at display row `row` of the Branches table.
    /// A click on a group heading selects nothing: headings are chrome, and
    /// moving the cursor to a neighbour would be a click landing somewhere the
    /// user didn't aim.
    fn select_branch_row(&mut self, row: usize) {
        if let Some(BranchRow::Branch(index)) = branch_display_rows(&self.branches).get(row) {
            self.branch_selected = *index;
        }
    }

    /// Key handling for the Branches tab (active when `view` is `List` and
    /// `tab` is `Branches`).
    fn on_branches_tab_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Down | KeyCode::Char('j') => {
                if self.branch_selected + 1 < self.branches.len() {
                    self.branch_selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.branch_selected = self.branch_selected.saturating_sub(1)
            }
            KeyCode::Char('r') => {
                self.load_branches(self.branch_selected);
                self.message = Some("refreshed".to_string());
            }
            // `f` refreshes every branch's ahead/behind against the remotes;
            // `p` then fast-forwards the selected one onto its upstream.
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('p') => self.start_branch_pull(),
            KeyCode::Char('n') => self.push_prompt(
                "new branch (no worktree)",
                TextInput::default(),
                "branch only, from HEAD · Esc cancels",
                ModalAction::BranchCreate,
            ),
            KeyCode::Char('R') => {
                if let Some(name) = self
                    .branches
                    .get(self.branch_selected)
                    .map(|b| b.name.clone())
                {
                    self.push_prompt(
                        "rename branch",
                        TextInput::with_value(name.clone()),
                        "new branch name · Esc cancels",
                        ModalAction::BranchRename { old: name },
                    );
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = self
                    .branches
                    .get(self.branch_selected)
                    .map(|b| b.name.clone())
                {
                    self.open_branch_delete_modal(name);
                }
            }
            // Enter drills into the branch's commit history, the entry point
            // for cherry-picking commits into a worktree.
            KeyCode::Enter => self.open_branch_commits(),
            // `m` merges the selected branch into a worktree of the user's
            // choosing, routing any conflicts into the resolver.
            KeyCode::Char('m') => self.open_merge_pick(),
            // `b` is the mirror: rebase a worktree of the user's choosing onto
            // the selected branch, also routing conflicts into the resolver.
            KeyCode::Char('b') => self.open_rebase_pick(),
            // `u` changes (or clears) which remote branch this one tracks.
            KeyCode::Char('u') => self.open_upstream_pick(),
            // `c` checks the branch out in a new worktree (the old Enter action).
            KeyCode::Char('c') => {
                if let Some(b) = self.branches.get(self.branch_selected) {
                    if b.checked_out_path.is_some() {
                        let msg = format!("branch '{}' is already checked out", b.name);
                        self.message = Some(msg);
                    } else {
                        let branch = b.name.clone();
                        self.open_create_prefilled(branch);
                    }
                }
            }
            _ => {}
        }
    }

    /// Opens the commit history of the selected branch (Branches tab → Enter),
    /// from which commits can be marked and cherry-picked into a worktree.
    fn open_branch_commits(&mut self) {
        let Some(branch) = self
            .branches
            .get(self.branch_selected)
            .map(|b| b.name.clone())
        else {
            return;
        };
        match self.branch_log_lines(&branch) {
            Ok(lines) => {
                let selected = first_commit_row(&lines);
                self.push_screen(View::BranchCommits {
                    branch,
                    marked: vec![false; lines.len()],
                    lines,
                    selected,
                });
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Commit history of a branch as graph rows, honouring `log_mode`.
    fn branch_log_lines(&self, branch: &str) -> Result<Vec<GraphLine>, String> {
        commit_log_lines(&self.ctx, branch, branch, true, self.log_mode)
    }

    /// Key handling for the branch commit-history view: move the cursor, toggle
    /// commits for cherry-picking, and open the worktree picker.
    fn on_branch_commits_key(&mut self, key: KeyEvent) {
        let View::BranchCommits {
            lines,
            marked,
            selected,
            ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Back to the Branches tab, keeping the branch highlighted.
                self.navigate_back();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(i) = seek_commit_row(lines, *selected, true) {
                    *selected = i;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = seek_commit_row(lines, *selected, false) {
                    *selected = i;
                }
            }
            KeyCode::Char(' ') => {
                // Only commits can be picked; art-only rows ignore the toggle.
                if lines.get(*selected).is_some_and(|l| l.entry.is_some())
                    && let Some(m) = marked.get_mut(*selected)
                {
                    *m = !*m;
                }
            }
            KeyCode::Char('a') => {
                let all = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.entry.is_some())
                    .all(|(i, _)| marked[i]);
                for (i, line) in lines.iter().enumerate() {
                    marked[i] = !all && line.entry.is_some();
                }
            }
            // Enter drills into the commit (consistent with the worktree log);
            // cherry-picking the marked commits is `p` for "pick".
            KeyCode::Enter | KeyCode::Char('v') | KeyCode::Right => {
                self.open_commit_diff_from_branch()
            }
            KeyCode::Char('p') => self.open_cherry_pick(),
            KeyCode::Char('t') => self.toggle_log_mode(),
            _ => {}
        }
    }

    /// Opens the read-only commit browser for the commit highlighted in a
    /// branch's history. The commit is viewed from the main worktree since a
    /// branch's commits are shared across the repo regardless of checkout.
    fn open_commit_diff_from_branch(&mut self) {
        let View::BranchCommits {
            lines, selected, ..
        } = &self.view
        else {
            return;
        };
        let Some(entry) = lines.get(*selected).and_then(|l| l.entry.as_ref()) else {
            return;
        };
        let Some(vantage) = self.worktrees.iter().find(|w| w.is_main) else {
            self.set_error("no main worktree to view the commit from");
            return;
        };
        let name = vantage.name.clone();
        let hash = entry.hash.clone();
        let label = format!(
            "{} {}",
            entry.hash.chars().take(9).collect::<String>(),
            entry.subject
        );
        self.open_commit_diff(name, hash, label);
    }

    /// Builds the cherry-pick worktree picker from the marked commits (or the
    /// one under the cursor when none are marked). Commits are ordered
    /// oldest-first, the order git applies them.
    fn open_cherry_pick(&mut self) {
        let View::BranchCommits {
            branch,
            lines,
            marked,
            selected,
        } = &self.view
        else {
            return;
        };
        // Gather chosen commits newest-first as they appear, then reverse to
        // oldest-first for git. Art-only rows carry no commit and drop out.
        let chosen: Vec<usize> = if marked.iter().any(|m| *m) {
            (0..lines.len()).filter(|i| marked[*i]).collect()
        } else {
            vec![*selected]
        };
        let mut commits: Vec<String> = Vec::new();
        let mut summaries: Vec<String> = Vec::new();
        for &i in chosen.iter().rev() {
            if let Some(e) = lines.get(i).and_then(|l| l.entry.as_ref()) {
                commits.push(e.hash.clone());
                summaries.push(e.subject.clone());
            }
        }
        if commits.is_empty() {
            return;
        }
        let source_branch = branch.clone();
        // Every existing worktree is a possible destination; cherry-pick needs a
        // working directory to apply into.
        let targets: Vec<CherryTarget> = self
            .worktrees
            .iter()
            .map(|w| CherryTarget {
                name: w.name.clone(),
                branch: w.branch.clone(),
            })
            .collect();
        if targets.is_empty() {
            self.message = Some("no worktrees to cherry-pick into".to_string());
            return;
        }
        self.push_screen(View::CherryPick {
            source_branch,
            commits,
            summaries,
            targets,
            selected: 0,
            mode: None,
        });
    }

    /// Key handling for the cherry-pick flow: pick a target worktree, then
    /// choose whether to commit or just load the changes, then run it.
    fn on_cherry_pick_key(&mut self, key: KeyEvent) {
        let View::CherryPick {
            targets,
            selected,
            mode,
            ..
        } = &mut self.view
        else {
            return;
        };
        match mode {
            // Mode prompt: commit vs load-only.
            Some(m) => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    *m = 1 - *m;
                }
                KeyCode::Enter => self.run_cherry_pick(),
                KeyCode::Esc | KeyCode::Char('q') => *mode = None,
                _ => {}
            },
            // Worktree picker.
            None => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected + 1 < targets.len() {
                        *selected += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Enter => *mode = Some(0),
                KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
                _ => {}
            },
        }
    }

    /// Runs the chosen cherry-pick in the background, returning to the Branches
    /// tab with a result message.
    fn run_cherry_pick(&mut self) {
        let View::CherryPick {
            commits,
            targets,
            selected,
            mode,
            ..
        } = &self.view
        else {
            return;
        };
        let Some(target) = targets.get(*selected) else {
            return;
        };
        let no_commit = *mode == Some(1);
        let target_name = target.name.clone();
        let commits = commits.clone();
        let count = commits.len();
        let verb = if no_commit {
            "loading"
        } else {
            "cherry-picking"
        };
        let label = if count == 1 {
            "the cherry-picked commit".to_string()
        } else {
            format!("{count} cherry-picked commits")
        };
        self.start_busy(
            format!("{verb} {count} commit(s) into '{target_name}'…"),
            BusyThen::Resolve {
                target: target_name.clone(),
                source_label: label,
                kind: ops::ResolveKind::CherryPick,
            },
            move |ctx| {
                ops::cherry_pick(ctx, &target_name, &commits, no_commit)
                    .map(|outcome| match outcome {
                        ops::CherryPickOutcome::Applied {
                            target,
                            count,
                            committed,
                        } => {
                            if committed {
                                format!("cherry-picked {count} commit(s) into '{target}'")
                            } else {
                                format!(
                                    "loaded {count} commit(s) into '{target}' (review, then commit)"
                                )
                            }
                        }
                        // The message is unused on conflict; the resolver opens.
                        ops::CherryPickOutcome::Conflicted { .. } => {
                            "conflicts to resolve".to_string()
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Opens the merge picker for the branch selected on the Branches tab,
    /// listing every worktree the branch can be merged into.
    fn open_merge_pick(&mut self) {
        let Some(source_branch) = self
            .branches
            .get(self.branch_selected)
            .map(|b| b.name.clone())
        else {
            return;
        };
        let targets: Vec<CherryTarget> = self
            .worktrees
            .iter()
            .map(|w| CherryTarget {
                name: w.name.clone(),
                branch: w.branch.clone(),
            })
            .collect();
        if targets.is_empty() {
            self.message = Some("no worktrees to merge into".to_string());
            return;
        }
        self.push_screen(View::MergePick {
            source_branch,
            targets,
            selected: 0,
        });
    }

    /// Opens the rebase picker for the branch selected on the Branches tab:
    /// choose which worktree to replay onto it.
    fn open_rebase_pick(&mut self) {
        let Some(onto_branch) = self
            .branches
            .get(self.branch_selected)
            .map(|b| b.name.clone())
        else {
            return;
        };
        // A worktree already on that branch has nothing to replay, so leave it
        // out rather than offering a choice git would refuse.
        let targets: Vec<CherryTarget> = self
            .worktrees
            .iter()
            .filter(|w| w.branch.as_deref() != Some(onto_branch.as_str()))
            .map(|w| CherryTarget {
                name: w.name.clone(),
                branch: w.branch.clone(),
            })
            .collect();
        if targets.is_empty() {
            self.message = Some(format!("no worktree to rebase onto '{onto_branch}'"));
            return;
        }
        self.push_screen(View::RebasePick {
            onto_branch,
            targets,
            selected: 0,
        });
    }

    /// Key handling for the rebase picker: pick the worktree to replay, then
    /// run the rebase in the background.
    fn on_rebase_pick_key(&mut self, key: KeyEvent) {
        let View::RebasePick {
            targets, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < targets.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.run_rebase(),
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            _ => {}
        }
    }

    /// Rebases the chosen worktree onto the picked branch on a background
    /// thread. Conflicts route into the resolver via `BusyThen::Resolve`.
    fn run_rebase(&mut self) {
        let picked = match &self.view {
            View::RebasePick {
                onto_branch,
                targets,
                selected,
            } => targets
                .get(*selected)
                .map(|t| (onto_branch.clone(), t.name.clone())),
            _ => None,
        };
        let Some((onto, target_name)) = picked else {
            return;
        };
        // Owned copies for the background closure (which outlives this frame).
        let tn = target_name.clone();
        let ob = onto.clone();
        self.start_busy(
            format!("rebasing '{target_name}' onto '{onto}'…"),
            BusyThen::Resolve {
                target: target_name,
                source_label: onto,
                kind: ops::ResolveKind::Rebase,
            },
            move |ctx| {
                ops::rebase(ctx, &tn, &ob, false)
                    .map(|outcome| match outcome {
                        ops::MergeOutcome::UpToDate => {
                            format!("'{tn}' is already on top of '{ob}'")
                        }
                        ops::MergeOutcome::Clean { commit } => {
                            format!("rebased '{tn}' onto '{ob}' ({commit})")
                        }
                        ops::MergeOutcome::FastForwarded { commit } => {
                            format!("fast-forwarded '{tn}' ({commit})")
                        }
                        // The message is unused on conflict; the resolver opens.
                        ops::MergeOutcome::Conflicted { .. } => "conflicts to resolve".to_string(),
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Key handling for the merge picker: pick a target worktree, then run the
    /// merge in the background.
    fn on_merge_pick_key(&mut self, key: KeyEvent) {
        let View::MergePick {
            targets, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < targets.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.run_merge(),
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            _ => {}
        }
    }

    /// Merges the picked branch into the chosen worktree on a background
    /// thread. Conflicts route into the resolver via `BusyThen::Resolve`.
    fn run_merge(&mut self) {
        let picked = match &self.view {
            View::MergePick {
                source_branch,
                targets,
                selected,
            } => targets
                .get(*selected)
                .map(|t| (source_branch.clone(), t.name.clone(), t.branch.clone())),
            _ => None,
        };
        let Some((source, target_name, target_branch)) = picked else {
            return;
        };
        // Merging a branch into the worktree that already has it checked out is
        // a no-op git would refuse; guard so the user gets a clear message.
        if target_branch.as_deref() == Some(source.as_str()) {
            self.message = Some(format!("'{target_name}' is already on '{source}'"));
            return;
        }
        // Owned copies for the background closure (which outlives this frame).
        let tn = target_name.clone();
        let src = source.clone();
        self.start_busy(
            format!("merging '{source}' into '{target_name}'…"),
            BusyThen::Resolve {
                target: target_name,
                source_label: source,
                kind: ops::ResolveKind::Merge,
            },
            move |ctx| {
                ops::merge(ctx, &tn, &src, false, false)
                    .map(|outcome| match outcome {
                        ops::MergeOutcome::UpToDate => format!("'{tn}' already up to date"),
                        ops::MergeOutcome::Clean { commit } => {
                            format!("merged '{src}' into '{tn}' ({commit})")
                        }
                        ops::MergeOutcome::FastForwarded { commit } => {
                            format!("fast-forwarded '{tn}' ({commit})")
                        }
                        // The message is unused on conflict; the resolver opens.
                        ops::MergeOutcome::Conflicted { .. } => "conflicts to resolve".to_string(),
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Opens the move-changes picker for the selected worktree, listing every
    /// other worktree as a possible destination.
    fn open_move_changes_pick(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        if wt.dirty == 0 {
            self.message = Some(format!("'{}' has no changes to move", wt.name));
            return;
        }
        let from = wt.name.clone();
        let targets: Vec<CherryTarget> = self
            .worktrees
            .iter()
            .filter(|w| w.name != from)
            .map(|w| CherryTarget {
                name: w.name.clone(),
                branch: w.branch.clone(),
            })
            .collect();
        if targets.is_empty() {
            self.message = Some("no other worktree to move changes into".to_string());
            return;
        }
        self.push_screen(View::MoveChanges {
            from,
            targets,
            selected: 0,
        });
    }

    /// Key handling for the move-changes picker: pick a target worktree, then
    /// run the move in the background.
    fn on_move_changes_key(&mut self, key: KeyEvent) {
        let View::MoveChanges {
            targets, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < targets.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Enter => self.run_move_changes(),
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            _ => {}
        }
    }

    /// Moves the selected worktree's uncommitted changes into the chosen
    /// worktree on a background thread (stash push + apply, see
    /// `ops::move_changes`).
    fn run_move_changes(&mut self) {
        let picked = match &self.view {
            View::MoveChanges {
                from,
                targets,
                selected,
            } => targets
                .get(*selected)
                .map(|t| (from.clone(), t.name.clone())),
            _ => None,
        };
        let Some((from, to)) = picked else {
            return;
        };
        self.start_busy(
            format!("moving changes from '{from}' into '{to}'…"),
            BusyThen::List,
            move |ctx| {
                ops::move_changes(ctx, &from, &to)
                    .map(|r| {
                        format!(
                            "moved {} change(s) from '{}' to '{}'",
                            r.files, r.from, r.to
                        )
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Refreshes the default branch from its upstream, then merges it into the
    /// selected worktree ("update from main"), or fast-forwards in place when
    /// the selection is already on the default branch. Conflicts route into
    /// the resolver.
    fn start_update(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        // A dirty worktree can't be merged into cleanly: git refuses when local
        // edits overlap the update. Offer to stash those changes, update, then
        // reapply them (git's --autostash) instead of failing outright.
        if wt.dirty > 0 {
            let (name, dirty) = (wt.name.clone(), wt.dirty);
            self.open_update_stash_modal(name, dirty);
            return;
        }
        self.run_update(wt.name.clone(), false);
    }

    /// Updates the worktree named `name` in the background (refresh default,
    /// then merge or fast-forward). With `autostash`, local changes are stashed
    /// before a merge and re-applied after (including after resolving conflicts).
    fn run_update(&mut self, name: String, autostash: bool) {
        let n = name.clone();
        self.start_busy(
            format!("updating '{name}' from the default branch…"),
            BusyThen::Resolve {
                target: name,
                source_label: "the default branch".to_string(),
                kind: ops::ResolveKind::Merge,
            },
            move |ctx| {
                ops::update(ctx, &n, autostash)
                    .map(|outcome| match outcome {
                        ops::MergeOutcome::UpToDate => format!("'{n}' already up to date"),
                        ops::MergeOutcome::Clean { commit } => format!("updated '{n}' ({commit})"),
                        ops::MergeOutcome::FastForwarded { commit } => {
                            format!("fast-forwarded '{n}' ({commit})")
                        }
                        ops::MergeOutcome::Conflicted { .. } => "conflicts to resolve".to_string(),
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// After a merge/update/cherry-pick/stash-pop op settles, opens the resolver
    /// when the target still has conflicts, otherwise shows the op's clean-result
    /// `msg`. A clean stash pop reloads the stash tab so the user can keep
    /// working there.
    fn finish_merge_op(
        &mut self,
        target: String,
        source_label: String,
        kind: ops::ResolveKind,
        msg: String,
    ) {
        match ops::list_conflicts(&self.ctx, &target) {
            Ok(files) if !files.is_empty() => self.open_resolver(target, source_label, kind, files),
            Ok(_) => {
                self.message = Some(msg);
                if matches!(kind, ops::ResolveKind::StashPop { .. }) {
                    self.reload_stash_tab(target);
                }
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Opens the conflict resolver on `target` for the given conflicted
    /// `files`, loading the first file's contents. `kind` records which
    /// operation the resolver will finish.
    fn open_resolver(
        &mut self,
        target: String,
        source_label: String,
        kind: ops::ResolveKind,
        files: Vec<String>,
    ) {
        let resolved = vec![false; files.len()];
        self.view = View::ConflictResolver {
            target,
            source_label,
            kind,
            files,
            resolved,
            file: 0,
            current: None,
        };
        self.load_resolver_file();
    }

    /// Reopens the resolver for the selected worktree when it is stopped in the
    /// middle of a merge, rebase, or cherry-pick. This is the way back in after
    /// leaving with `q`, and the only way to reach a conflict that started
    /// outside wtm (a `git rebase` in a terminal, say).
    fn open_resolver_for_selected(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let target = wt.name.clone();
        let kind = match ops::detect_resolve_kind(&self.ctx, &target) {
            Ok(Some(kind)) => kind,
            Ok(None) => {
                self.message = Some(format!("no merge, rebase, or cherry-pick in '{target}'"));
                return;
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        let files = match ops::list_conflicts(&self.ctx, &target) {
            Ok(files) => files,
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        // An operation with nothing left unmerged still belongs in the
        // resolver: it needs the "complete" step, and there is nowhere else to
        // do that from. `q` leaves via `go_root`, so nothing needs pushing.
        let label = format!("{} in progress", kind.label());
        self.open_resolver(target, label, kind, files);
    }

    /// Loads (or reloads) the currently selected conflicted file into the
    /// resolver, parsing it into hunks with every hunk left unresolved. A file
    /// with no remaining conflict markers (already resolved) or a read error
    /// leaves `current` empty, which the renderer shows as "resolved".
    fn load_resolver_file(&mut self) {
        let target_path = match &self.view {
            View::ConflictResolver {
                target,
                files,
                file,
                ..
            } => files.get(*file).map(|p| (target.clone(), p.clone())),
            _ => None,
        };
        let Some((target, path)) = target_path else {
            return;
        };
        let loaded = ops::read_conflict(&self.ctx, &target, &path)
            .ok()
            .and_then(|cf| {
                let hunks = cf
                    .segments
                    .iter()
                    .filter(|s| matches!(s, ConflictSegment::Hunk { .. }))
                    .count();
                // A file with no hunks left is fully resolved; show nothing.
                (hunks > 0).then(|| ResolverFile {
                    file: cf,
                    actions: vec![None; hunks],
                    hunk: 0,
                })
            });
        if let View::ConflictResolver { current, .. } = &mut self.view {
            *current = loaded;
        }
    }

    /// Key handling for the conflict resolver.
    fn on_resolver_key(&mut self, key: KeyEvent) {
        match key.code {
            // Leaving keeps the merge in progress so it can be resumed later.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.go_root();
                self.arm_ignore_quit();
                self.refresh();
            }
            KeyCode::Left | KeyCode::Char('[') | KeyCode::Char('h') => self.resolver_move_file(-1),
            KeyCode::Right | KeyCode::Char(']') | KeyCode::Char('l') => self.resolver_move_file(1),
            KeyCode::Down | KeyCode::Char('j') => self.resolver_move_hunk(1),
            KeyCode::Up | KeyCode::Char('k') => self.resolver_move_hunk(-1),
            KeyCode::Char('o') => self.resolver_set_action(ResolutionAction::KeepOurs),
            KeyCode::Char('t') => self.resolver_set_action(ResolutionAction::KeepTheirs),
            KeyCode::Char('b') => self.resolver_set_action(ResolutionAction::KeepBoth),
            KeyCode::Char('B') => self.resolver_set_action(ResolutionAction::KeepBothReversed),
            KeyCode::Char('O') => self.resolver_whole_file(true),
            KeyCode::Char('T') => self.resolver_whole_file(false),
            KeyCode::Char('e') => self.resolver_edit_hunk(),
            KeyCode::Char('E') => self.resolver_edit_file(),
            KeyCode::Char('w') | KeyCode::Enter => self.resolver_write_file(),
            KeyCode::Char('a') => self.resolver_stage_as_is(),
            KeyCode::Char('r') => self.resolver_reload(),
            KeyCode::Char('s') => self.resolver_skip_commit(),
            KeyCode::Char('c') => self.resolver_complete(),
            KeyCode::Char('x') => {
                if let View::ConflictResolver { target, .. } = &self.view {
                    let target = target.clone();
                    self.open_resolver_abort_modal(target);
                }
            }
            _ => {}
        }
    }

    /// Moves the file cursor by `delta`, clamped, and reloads the new file.
    fn resolver_move_file(&mut self, delta: isize) {
        let moved = if let View::ConflictResolver { files, file, .. } = &mut self.view {
            let n = files.len();
            if n == 0 {
                false
            } else {
                let new = (*file as isize + delta).clamp(0, n as isize - 1) as usize;
                let moved = new != *file;
                *file = new;
                moved
            }
        } else {
            false
        };
        if moved {
            self.load_resolver_file();
        }
    }

    /// Moves the hunk cursor within the current file by `delta`, clamped.
    fn resolver_move_hunk(&mut self, delta: isize) {
        if let View::ConflictResolver {
            current: Some(rf), ..
        } = &mut self.view
        {
            let n = rf.actions.len();
            if n > 0 {
                rf.hunk = (rf.hunk as isize + delta).clamp(0, n as isize - 1) as usize;
            }
        }
    }

    /// Records `action` for the current hunk of the current file.
    fn resolver_set_action(&mut self, action: ResolutionAction) {
        if let View::ConflictResolver {
            current: Some(rf), ..
        } = &mut self.view
            && let Some(slot) = rf.actions.get_mut(rf.hunk)
        {
            *slot = Some(action);
        }
    }

    /// Opens the manual editor for the current hunk. It is seeded from the
    /// side already chosen (if any), else from both sides so nothing is lost;
    /// the user then trims or rewrites it into the final result.
    fn resolver_edit_hunk(&mut self) {
        if let View::ConflictResolver {
            current: Some(rf), ..
        } = &mut self.view
        {
            let seed = rf
                .file
                .segments
                .iter()
                .filter_map(|s| match s {
                    ConflictSegment::Hunk { ours, theirs, .. } => Some((ours, theirs)),
                    _ => None,
                })
                .nth(rf.hunk)
                .map(
                    |(ours, theirs)| match rf.actions.get(rf.hunk).and_then(|a| a.clone()) {
                        Some(ResolutionAction::KeepOurs) => ours.clone(),
                        Some(ResolutionAction::KeepTheirs) => theirs.clone(),
                        Some(ResolutionAction::KeepBothReversed) => format!("{theirs}{ours}"),
                        Some(ResolutionAction::Manual(t)) => t,
                        // No side picked, or "keep both": start with both, ours first.
                        _ => format!("{ours}{theirs}"),
                    },
                );
            if let Some(text) = seed {
                self.modal = Some(Modal::HunkEditor(TextArea::new(&text)));
            }
        }
    }

    /// Opens the whole conflicted file in the editor, conflict markers and all,
    /// for the fixes per-hunk choices can't express. Decisions already made in
    /// the resolver are flushed to disk first, so the text on screen is the
    /// file as it really stands rather than a stale copy that would silently
    /// throw that work away on save.
    fn resolver_edit_file(&mut self) {
        // Only when there is something to flush: writing the file back
        // unprompted would relabel every conflict marker for no reason.
        let pending = matches!(
            &self.view,
            View::ConflictResolver { current: Some(rf), .. } if rf.actions.iter().any(Option::is_some)
        );
        if pending {
            self.resolver_save_current_file(false);
        }
        let target_path = match &self.view {
            View::ConflictResolver {
                target,
                files,
                file,
                ..
            } => files.get(*file).map(|p| (target.clone(), p.clone())),
            _ => None,
        };
        let Some((target, path)) = target_path else {
            return;
        };
        match ops::read_worktree_file(&self.ctx, &target, &path) {
            Ok(text) => {
                self.modal = Some(Modal::FileEditor {
                    path,
                    editor: TextArea::new(&text),
                });
                self.message = Some(
                    "editing the whole file · Ctrl+S saves, Esc discards \
                     (delete the <<<<<<< / ======= / >>>>>>> markers as you go)"
                        .to_string(),
                );
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Key handling while the whole-file editor is open: Ctrl+S writes the file
    /// to disk and re-reads it, Esc discards, everything else edits.
    fn on_file_editor_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.resolver_save_file_edit();
            return;
        }
        if key.code == KeyCode::Esc {
            self.modal = None;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        if let Some(Modal::FileEditor { editor, .. }) = &mut self.modal {
            editor.on_key(key);
        }
    }

    /// Writes the whole-file edit to disk (without staging) and re-reads the
    /// file, so hunks the edit settled drop out of the resolver and any left
    /// behind renumber. Staging stays a separate, deliberate step (`w`/`a`).
    fn resolver_save_file_edit(&mut self) {
        let edit = match &self.modal {
            Some(Modal::FileEditor { path, editor, .. }) => Some((path.clone(), editor.text())),
            _ => None,
        };
        let Some((path, text)) = edit else {
            return;
        };
        let target = match &self.view {
            View::ConflictResolver { target, .. } => target.clone(),
            _ => return,
        };
        match ops::write_partial_resolution(&self.ctx, &target, &path, &text) {
            Ok(()) => {
                self.modal = None;
                self.load_resolver_file();
                let remaining = match &self.view {
                    View::ConflictResolver {
                        current: Some(rf), ..
                    } => rf.actions.len(),
                    _ => 0,
                };
                self.message = Some(if remaining == 0 {
                    format!("saved '{path}' · no conflict markers left, press w to stage it")
                } else {
                    format!("saved '{path}' · {remaining} hunk(s) still conflicted")
                });
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Key handling while the manual hunk editor modal is open: Ctrl+S saves the
    /// edit as a `Manual` resolution, Esc discards it, everything else edits.
    fn on_hunk_editor_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.resolver_save_manual_edit();
            return;
        }
        if key.code == KeyCode::Esc {
            self.modal = None;
            return;
        }
        // Any other Ctrl-chord is not an edit: without this guard Ctrl+A and
        // friends insert their bare letter into the buffer.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        if let Some(Modal::HunkEditor(ed)) = &mut self.modal {
            ed.on_key(key);
        }
    }

    /// Saves the open manual edit as the current hunk's resolution, closes the
    /// editor, and writes the file straight out to disk. Persisting immediately
    /// is the whole point: an edit kept only in memory is invisible to git and
    /// to any other tool, and used to be silently discarded on moving to
    /// another file. Undecided hunks keep their conflict markers, so this never
    /// stages a half-resolved file.
    fn resolver_save_manual_edit(&mut self) {
        let text = match &self.modal {
            Some(Modal::HunkEditor(ed)) => ed.text(),
            _ => return,
        };
        self.modal = None;
        if let View::ConflictResolver {
            current: Some(rf), ..
        } = &mut self.view
            && let Some(slot) = rf.actions.get_mut(rf.hunk)
        {
            *slot = Some(ResolutionAction::Manual(text));
        }
        self.resolver_save_current_file(false);
    }

    /// Writes the current file and, when `stage` is set and every hunk has been
    /// decided, stages it as resolved and moves to the next file.
    ///
    /// A file with hunks still undecided is written back with those hunks
    /// wrapped in their original conflict markers, so the work done so far
    /// survives on disk while git rightly still sees the path as unmerged. The
    /// file is then re-read, which drops the settled hunks from the hunk list.
    fn resolver_save_current_file(&mut self, stage: bool) {
        let prepared = if let View::ConflictResolver {
            target,
            files,
            file,
            current,
            ..
        } = &self.view
        {
            current.as_ref().and_then(|rf| {
                let path = files.get(*file)?.clone();
                let text = conflict::render_partial(
                    &rf.file.segments,
                    &rf.actions,
                    &rf.file.ours_label,
                    &rf.file.theirs_label,
                );
                let undecided = rf.actions.iter().filter(|a| a.is_none()).count();
                Some((target.clone(), path, text, undecided))
            })
        } else {
            None
        };
        let Some((target, path, text, undecided)) = prepared else {
            // No hunks parsed: the file is already free of conflict markers, so
            // the only thing left to do is mark it resolved.
            if stage {
                self.resolver_stage_as_is();
            }
            return;
        };
        if stage && undecided == 0 {
            match ops::write_resolution(&self.ctx, &target, &path, &text) {
                Ok(()) => {
                    self.message = Some(format!("staged '{path}'"));
                    self.resolver_mark_resolved_and_advance();
                }
                Err(e) => self.set_error(format!("{e:#}")),
            }
            return;
        }
        match ops::write_partial_resolution(&self.ctx, &target, &path, &text) {
            Ok(()) => {
                self.message = Some(if undecided == 0 {
                    format!("saved '{path}' · press w to stage it as resolved")
                } else {
                    format!(
                        "saved '{path}' · {undecided} hunk(s) still conflicted \
                         (pick a side with o/t/b, or edit with e)"
                    )
                });
                // Re-read so the hunks just settled drop out of the list and the
                // remaining ones renumber.
                self.load_resolver_file();
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Writes the current file from its chosen per-hunk actions and stages it
    /// as resolved, then advances to the next unresolved file. With hunks still
    /// undecided the work is saved to disk but the file is left unstaged, since
    /// staging conflict markers would commit them.
    fn resolver_write_file(&mut self) {
        self.resolver_save_current_file(true);
    }

    /// Marks the current file resolved by staging exactly what is on disk, for
    /// a conflict fixed outside wtm (in an editor, or another terminal). Fails
    /// loudly while conflict markers remain rather than staging them.
    fn resolver_stage_as_is(&mut self) {
        let target_path = match &self.view {
            View::ConflictResolver {
                target,
                files,
                file,
                ..
            } => files.get(*file).map(|p| (target.clone(), p.clone())),
            _ => None,
        };
        let Some((target, path)) = target_path else {
            return;
        };
        match ops::stage_resolved(&self.ctx, &target, &path) {
            Ok(()) => {
                self.message = Some(format!("marked '{path}' resolved"));
                self.resolver_mark_resolved_and_advance();
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Re-reads the conflict state from disk, so resolutions made outside wtm
    /// (an editor, a `git add` in another terminal) show up. Files that are no
    /// longer conflicted drop off the list; the cursor stays on the same path
    /// when it survives.
    fn resolver_reload(&mut self) {
        let target = match &self.view {
            View::ConflictResolver { target, .. } => target.clone(),
            _ => return,
        };
        let current_path = match &self.view {
            View::ConflictResolver { files, file, .. } => files.get(*file).cloned(),
            _ => None,
        };
        let files = match ops::list_conflicts(&self.ctx, &target) {
            Ok(files) => files,
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        if files.is_empty() {
            if let View::ConflictResolver {
                files: f,
                resolved,
                file,
                current,
                ..
            } = &mut self.view
            {
                f.clear();
                resolved.clear();
                *file = 0;
                *current = None;
            }
            self.message = Some("every conflict is resolved · press c to finish".to_string());
            return;
        }
        let keep = current_path
            .and_then(|p| files.iter().position(|f| *f == p))
            .unwrap_or(0);
        if let View::ConflictResolver {
            files: f,
            resolved,
            file,
            ..
        } = &mut self.view
        {
            *resolved = vec![false; files.len()];
            *f = files;
            *file = keep;
        }
        self.load_resolver_file();
        self.message = Some("reloaded conflicts from disk".to_string());
    }

    /// Takes the whole current file from one side (ours or theirs) and stages
    /// it, then advances to the next unresolved file.
    fn resolver_whole_file(&mut self, ours: bool) {
        let target_path = match &self.view {
            View::ConflictResolver {
                target,
                files,
                file,
                ..
            } => files.get(*file).map(|p| (target.clone(), p.clone())),
            _ => None,
        };
        let Some((target, path)) = target_path else {
            return;
        };
        let res = if ours {
            ops::checkout_ours(&self.ctx, &target, &path)
        } else {
            ops::checkout_theirs(&self.ctx, &target, &path)
        };
        match res {
            Ok(()) => {
                let side = if ours { "ours" } else { "theirs" };
                self.message = Some(format!("took {side} for '{path}'"));
                self.resolver_mark_resolved_and_advance();
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Marks the current file resolved, then jumps to the next still-unresolved
    /// file (wrapping around), reloading its contents.
    fn resolver_mark_resolved_and_advance(&mut self) {
        let next = if let View::ConflictResolver { resolved, file, .. } = &mut self.view {
            if let Some(r) = resolved.get_mut(*file) {
                *r = true;
            }
            let n = resolved.len();
            (1..=n).map(|off| (*file + off) % n).find(|&i| !resolved[i])
        } else {
            None
        };
        if let (Some(i), View::ConflictResolver { file, .. }) = (next, &mut self.view) {
            *file = i;
        }
        self.load_resolver_file();
    }

    /// Finishes the resolved operation (commit the merge, continue the
    /// cherry-pick, or drop the popped stash) and returns to the worktree list.
    /// Errors (e.g. conflicts still unresolved) surface in the modal error popup.
    fn resolver_complete(&mut self) {
        let (target, kind) = match &self.view {
            View::ConflictResolver { target, kind, .. } => (target.clone(), *kind),
            _ => return,
        };
        match ops::complete_resolution(&self.ctx, &target, kind, None) {
            Ok(r) => {
                self.go_root();
                self.refresh();
                self.message = Some(match r.commit {
                    Some(commit) => format!("resolved '{}' ({commit})", r.target),
                    None => format!("resolved '{}'", r.target),
                });
                // Completing a stash pop drops the stash, so the Stash tab the
                // resolver was opened from would otherwise still list it.
                if matches!(kind, ops::ResolveKind::StashPop { .. }) {
                    self.reload_stash_tab(target);
                }
            }
            // The usual reason completing is refused is a file still unmerged,
            // often one the user fixed in their own editor without staging it.
            // Point at the keys that deal with that instead of just relaying
            // git's complaint.
            Err(e) => {
                let hint = match ops::list_conflicts(&self.ctx, &target) {
                    Ok(files) if !files.is_empty() => {
                        "\n\nstage a file you resolved yourself with a, \
                         or press r to re-read the conflicts from disk"
                    }
                    _ => "",
                };
                self.set_error(format!("{e:#}{hint}"));
            }
        }
    }

    /// Drops the commit a rebase stopped on and carries on with the rest.
    /// Only a rebase has anything to skip; other kinds report why not.
    fn resolver_skip_commit(&mut self) {
        let (target, kind) = match &self.view {
            View::ConflictResolver { target, kind, .. } => (target.clone(), *kind),
            _ => return,
        };
        if kind != ops::ResolveKind::Rebase {
            self.message = Some(format!(
                "nothing to skip: this is a {}, not a rebase",
                kind.label()
            ));
            return;
        }
        match ops::skip_resolution(&self.ctx, &target, kind) {
            Ok(()) => {
                // Skipping either finishes the rebase or stops on the next
                // commit, so re-read rather than assuming either.
                if ops::detect_resolve_kind(&self.ctx, &target)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    self.message = Some("skipped the commit".to_string());
                    self.resolver_reload();
                } else {
                    self.go_root();
                    self.refresh();
                    self.message = Some(format!("rebase of '{target}' finished"));
                }
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Aborts the in-progress operation and returns to the worktree list.
    fn abort_resolver(&mut self) {
        let (target, kind) = match &self.view {
            View::ConflictResolver { target, kind, .. } => (target.clone(), *kind),
            _ => return,
        };
        match ops::abort_resolution(&self.ctx, &target, kind) {
            Ok(()) => {
                self.go_root();
                self.refresh();
                self.message = Some(format!("aborted resolution in '{target}'"));
                // An aborted stash pop leaves the stash in place; re-read it so
                // the Stash tab behind the resolver matches.
                if matches!(kind, ops::ResolveKind::StashPop { .. }) {
                    self.reload_stash_tab(target);
                }
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Creates a branch from HEAD and reloads the Branches tab.
    fn branch_create(&mut self, name: String) {
        self.start_busy(
            format!("creating branch '{name}'…"),
            BusyThen::Branch,
            move |ctx| {
                ops::branch_create(ctx, &name, None)
                    .map(|_| format!("created branch '{name}'"))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Renames the selected branch, then reloads the Branches tab.
    fn branch_rename(&mut self, old: String, new: String) {
        match ops::branch_rename(&self.ctx, &old, &new) {
            Ok(r) => {
                self.message = Some(format!("renamed branch '{}' to '{}'", r.old, r.new));
                self.load_branches(self.branch_selected);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Deletes a branch. A refused non-force delete reopens the confirm (under
    /// the error popup) so the user can retry with `f` (force). Runs
    /// synchronously (a fast local op) so that retry flow stays intact.
    fn branch_delete(&mut self, name: String, force: bool) {
        match ops::branch_delete(&self.ctx, &name, force) {
            Ok(r) => {
                self.message = Some(format!(
                    "deleted branch '{}'{}",
                    r.name,
                    if r.forced { " (forced)" } else { "" }
                ));
                self.load_branches(self.branch_selected);
            }
            Err(e) => {
                self.set_error(format!("{e:#} — press f to force"));
                self.open_branch_delete_modal(name);
            }
        }
    }

    /// Opens the new-worktree dialog prefilled with `branch`, used when the
    /// Branches tab targets a branch that isn't checked out anywhere.
    fn open_create_prefilled(&mut self, branch: String) {
        self.open_create();
        // The branch browser picks an existing branch to check out, so select
        // it in the checkout list rather than the new-branch row.
        // No filter text is typed yet, so the filtered list equals `branches`
        // and the position maps straight to the checkout selection.
        if let View::Create {
            branches, selected, ..
        } = &mut self.view
            && let Some(pos) = branches.iter().position(|b| b.branch == branch)
        {
            *selected = pos + 1;
        }
    }

    /// Opens the scrollable commit log for the selected worktree, drawn in the
    /// current `log_mode`.
    fn open_log(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        match self.worktree_log_lines(&name) {
            Ok(lines) => {
                let selected = first_commit_row(&lines);
                self.push_screen(View::Log {
                    name,
                    lines,
                    selected,
                })
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Commit history of a worktree as graph rows, honouring `log_mode`.
    fn worktree_log_lines(&self, name: &str) -> Result<Vec<GraphLine>, String> {
        commit_log_lines(&self.ctx, name, name, false, self.log_mode)
    }

    fn on_log_key(&mut self, key: KeyEvent) {
        let View::Log {
            lines, selected, ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            // Move the cursor to the next/previous row that holds a commit,
            // skipping the art-only connector rows git draws between them.
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(next) = seek_commit_row(lines, *selected, true) {
                    *selected = next;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(prev) = seek_commit_row(lines, *selected, false) {
                    *selected = prev;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => *selected = first_commit_row(lines),
            // Open the commit browser for the commit under the cursor.
            KeyCode::Enter => self.open_commit_diff_from_log(),
            // Swap between the commit graph and the plain list, reloading in
            // place and returning to the top since the rows no longer line up.
            KeyCode::Char('t') => self.toggle_log_mode(),
            _ => {}
        }
    }

    /// Opens the read-only commit browser for the commit highlighted in the log.
    fn open_commit_diff_from_log(&mut self) {
        let View::Log {
            name,
            lines,
            selected,
        } = &self.view
        else {
            return;
        };
        let Some(entry) = lines.get(*selected).and_then(|l| l.entry.as_ref()) else {
            return;
        };
        let name = name.clone();
        let hash = entry.hash.clone();
        let label = format!(
            "{} {}",
            entry.hash.chars().take(9).collect::<String>(),
            entry.subject
        );
        self.open_commit_diff(name, hash, label);
    }

    /// Opens the read-only commit browser for `hash`, loading its changed-file
    /// list and the first file's diff (off-thread). Pushed onto the navigation
    /// stack so Esc returns to whichever log opened it (worktree or branch).
    fn open_commit_diff(&mut self, name: String, hash: String, label: String) {
        match ops::commit_files(&self.ctx, &name, &hash) {
            Ok(files) => {
                let rows = build_rows(&files, self.file_tree, &self.collapsed_folders);
                self.push_screen(View::CommitDiff {
                    name,
                    hash,
                    label,
                    files,
                    rows,
                    selected: 0,
                    content: String::new(),
                    content_path: None,
                    load_gen: 0,
                    pending: None,
                    loading_new: false,
                    scroll: 0,
                    h_scroll: 0,
                });
                self.load_file_browser_diff_content(true);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Key handling for the read-only commit/stash file browser: navigate files,
    /// scroll the diff (including horizontally), toggle tree/flat, or go back.
    fn on_file_browser_key(&mut self, key: KeyEvent) {
        let (rows_len, selected) = match &self.view {
            View::CommitDiff { rows, selected, .. } | View::StashDiff { rows, selected, .. } => {
                (rows.len(), *selected)
            }
            _ => return,
        };
        // Vertical scroll (Shift+↑/↓ or J/K).
        let shift_down = key.code == KeyCode::Down && key.modifiers.contains(KeyModifiers::SHIFT);
        let shift_up = key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::SHIFT);
        if shift_down || key.code == KeyCode::Char('J') {
            self.scroll_file_browser_v(|s| s.saturating_add(3));
            return;
        }
        if shift_up || key.code == KeyCode::Char('K') {
            self.scroll_file_browser_v(|s| s.saturating_sub(3));
            return;
        }
        // Horizontal scroll (Shift+←/→ or H/L). Plain h/l still tree-navigate.
        let shift_right = key.code == KeyCode::Right && key.modifiers.contains(KeyModifiers::SHIFT);
        let shift_left = key.code == KeyCode::Left && key.modifiers.contains(KeyModifiers::SHIFT);
        if shift_right || key.code == KeyCode::Char('L') {
            self.scroll_file_browser_h(|s| s.saturating_add(8));
            return;
        }
        if shift_left || key.code == KeyCode::Char('H') {
            self.scroll_file_browser_h(|s| s.saturating_sub(8));
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.navigate_back(),
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 < rows_len {
                    if let View::CommitDiff { selected, .. } | View::StashDiff { selected, .. } =
                        &mut self.view
                    {
                        *selected += 1;
                    }
                    self.load_file_browser_diff_content(true);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if selected > 0 {
                    if let View::CommitDiff { selected, .. } | View::StashDiff { selected, .. } =
                        &mut self.view
                    {
                        *selected -= 1;
                    }
                    self.load_file_browser_diff_content(true);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll_file_browser_v(|_| 0);
                self.scroll_file_browser_h(|_| 0);
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Enter => self.tree_nav(key.code),
            KeyCode::Char('t') => self.toggle_file_browser_layout(),
            _ => {}
        }
    }

    /// Opens a stash entry's file browser (files + per-file diffs).
    fn open_stash_diff(&mut self, name: String, index: u32, label: String) {
        match ops::stash_files(&self.ctx, &name, index) {
            Ok(files) => {
                let rows = build_rows(&files, self.file_tree, &self.collapsed_folders);
                self.push_screen(View::StashDiff {
                    name,
                    index,
                    label,
                    files,
                    rows,
                    selected: 0,
                    content: String::new(),
                    content_path: None,
                    load_gen: 0,
                    pending: None,
                    loading_new: false,
                    scroll: 0,
                    h_scroll: 0,
                });
                self.load_file_browser_diff_content(true);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Flips the commit/stash browser's file list between tree and flat.
    fn toggle_file_browser_layout(&mut self) {
        self.file_tree = !self.file_tree;
        let tree = self.file_tree;
        let (files, rows, selected) = match &mut self.view {
            View::CommitDiff {
                files,
                rows,
                selected,
                ..
            }
            | View::StashDiff {
                files,
                rows,
                selected,
                ..
            } => (files, rows, selected),
            _ => return,
        };
        let path = current_file_index(rows, *selected).map(|i| files[i].path.clone());
        *rows = build_rows(files, tree, &self.collapsed_folders);
        *selected = path
            .and_then(|p| {
                rows.iter().position(
                    |r| matches!(r, DiffRow::File { index, .. } if files[*index].path == p),
                )
            })
            .unwrap_or(0);
        self.load_file_browser_diff_content(true);
    }

    /// Applies `f` to the commit/stash browser's vertical scroll offset.
    fn scroll_file_browser_v(&mut self, f: impl FnOnce(u16) -> u16) {
        match &mut self.view {
            View::CommitDiff { scroll, .. } | View::StashDiff { scroll, .. } => {
                *scroll = f(*scroll);
            }
            _ => {}
        }
    }

    /// Applies `f` to the commit/stash browser's horizontal scroll offset.
    fn scroll_file_browser_h(&mut self, f: impl FnOnce(u16) -> u16) {
        match &mut self.view {
            View::CommitDiff {
                h_scroll, content, ..
            }
            | View::StashDiff {
                h_scroll, content, ..
            } => {
                let max = content
                    .lines()
                    .map(|l| l.chars().count())
                    .max()
                    .unwrap_or(0) as u16;
                *h_scroll = f(*h_scroll).min(max);
            }
            _ => {}
        }
    }

    /// Loads the diff for the file under the commit/stash browser's cursor.
    /// `reset_scroll` sends the viewport to the top-left when the file changes.
    fn load_file_browser_diff_content(&mut self, reset_scroll: bool) {
        enum Source {
            Commit { name: String, hash: String },
            Stash { name: String, index: u32 },
        }
        let (source, entry) = match &self.view {
            View::CommitDiff {
                name,
                hash,
                rows,
                files,
                selected,
                ..
            } => (
                Source::Commit {
                    name: name.clone(),
                    hash: hash.clone(),
                },
                current_file_index(rows, *selected).and_then(|i| files.get(i).cloned()),
            ),
            View::StashDiff {
                name,
                index,
                rows,
                files,
                selected,
                ..
            } => (
                Source::Stash {
                    name: name.clone(),
                    index: *index,
                },
                current_file_index(rows, *selected).and_then(|i| files.get(i).cloned()),
            ),
            _ => return,
        };
        let clear = |view: &mut View, reset_scroll: bool| {
            if let View::CommitDiff {
                content,
                content_path,
                pending,
                loading_new,
                scroll,
                h_scroll,
                ..
            }
            | View::StashDiff {
                content,
                content_path,
                pending,
                loading_new,
                scroll,
                h_scroll,
                ..
            } = view
            {
                content.clear();
                *content_path = None;
                *pending = None;
                *loading_new = false;
                if reset_scroll {
                    *scroll = 0;
                    *h_scroll = 0;
                }
            }
        };
        let Some(e) = entry else {
            clear(&mut self.view, reset_scroll);
            return;
        };
        let path = e.path.clone();
        let (token, is_new) = match &mut self.view {
            View::CommitDiff {
                load_gen,
                content_path,
                scroll,
                h_scroll,
                ..
            }
            | View::StashDiff {
                load_gen,
                content_path,
                scroll,
                h_scroll,
                ..
            } => {
                *load_gen = load_gen.wrapping_add(1);
                let is_new = content_path.as_deref() != Some(path.as_str());
                if reset_scroll {
                    *scroll = 0;
                    *h_scroll = 0;
                }
                (*load_gen, is_new)
            }
            _ => return,
        };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let path_for_thread = path.clone();
        std::thread::spawn(move || {
            let content = match source {
                Source::Commit { name, hash } => {
                    ops::commit_file_diff(&ctx, &name, &hash, &path_for_thread)
                }
                Source::Stash { name, index } => {
                    ops::stash_file_diff(&ctx, &name, index, &path_for_thread)
                }
            };
            let content = match content {
                Ok(c) => c,
                Err(err) => format!("error: {err:#}"),
            };
            let _ = tx.send((token, path_for_thread, content));
        });
        if let View::CommitDiff {
            pending,
            loading_new,
            ..
        }
        | View::StashDiff {
            pending,
            loading_new,
            ..
        } = &mut self.view
        {
            *pending = Some(Task::new(rx));
            *loading_new = is_new;
        }
    }

    /// Applies the newest background commit/stash-diff result when it still
    /// matches the current generation.
    fn poll_file_browser_diff_load(&mut self) {
        let (pending, load_gen) = match &self.view {
            View::CommitDiff {
                pending, load_gen, ..
            }
            | View::StashDiff {
                pending, load_gen, ..
            } => (pending, *load_gen),
            _ => return,
        };
        let Some(rx) = pending else {
            return;
        };
        let Some((g, path, content)) = rx.poll_latest() else {
            return;
        };
        if g != load_gen {
            return;
        }
        if let View::CommitDiff {
            content: slot,
            content_path,
            pending,
            loading_new,
            scroll,
            h_scroll,
            ..
        }
        | View::StashDiff {
            content: slot,
            content_path,
            pending,
            loading_new,
            scroll,
            h_scroll,
            ..
        } = &mut self.view
        {
            *slot = content;
            *content_path = Some(path);
            *pending = None;
            *loading_new = false;
            let max_v = slot.lines().count().saturating_sub(1) as u16;
            *scroll = (*scroll).min(max_v);
            let max_h = slot.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            *h_scroll = (*h_scroll).min(max_h);
        }
    }

    /// Flips `log_mode` and reloads whichever commit view is open.
    fn toggle_log_mode(&mut self) {
        self.log_mode = self.log_mode.toggled();
        match &self.view {
            View::Log { name, .. } => {
                let name = name.clone();
                match self.worktree_log_lines(&name) {
                    Ok(lines) => {
                        let selected = first_commit_row(&lines);
                        self.view = View::Log {
                            name,
                            lines,
                            selected,
                        }
                    }
                    Err(e) => self.set_error(e),
                }
            }
            // Any cherry-pick marks are dropped: the rows are re-derived and no
            // longer line up with the old ones.
            View::BranchCommits { branch, .. } => {
                let branch = branch.clone();
                match self.branch_log_lines(&branch) {
                    Ok(lines) => {
                        let selected = first_commit_row(&lines);
                        self.view = View::BranchCommits {
                            branch,
                            marked: vec![false; lines.len()],
                            lines,
                            selected,
                        };
                    }
                    Err(e) => self.set_error(e),
                }
            }
            View::List if self.worktree_commits.is_some() || self.commits_pending.is_some() => {
                let (name, branch, from_branch) = if let Some(panel) = &self.worktree_commits {
                    (panel.name.clone(), panel.branch.clone(), panel.from_branch)
                } else if let Some((key, _)) = &self.commits_pending {
                    (key.name.clone(), key.branch.clone(), key.from_branch)
                } else {
                    return;
                };
                // Clear so the UI shows loading rather than the other mode's rows.
                self.worktree_commits = None;
                self.commits_pending = None;
                self.load_worktree_commits_panel(name, branch, from_branch);
            }
            _ => return,
        }
        self.message = Some(format!("{} view", self.log_mode.label()));
    }

    /// Runs `op` on a background thread and shows the Busy overlay until
    /// tick() drains its result. Keeps long git ops off the UI thread. `then`
    /// picks which view is reopened once the op finishes.
    fn start_busy(
        &mut self,
        label: String,
        then: BusyThen,
        op: impl FnOnce(&Ctx) -> Result<String, String> + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(op(&ctx));
        });
        // The busy overlay and whatever its `then` reopens (the list, the stash
        // manager, the resolver) always land back at the root, so collapse the
        // stack rather than orphaning the screen that launched the op.
        self.stack.clear();
        self.view = View::Busy {
            label,
            rx: Task::new(rx),
            then,
        };
    }

    /// Pulls the selected worktree (fast-forward only) in the background. When
    /// the pull is refused because the branch has diverged, tick() opens the
    /// `ConfirmPullRebase` prompt instead of showing the error. On the Changes
    /// tab the worktree is the one whose diffs are open, not the Worktrees-tab
    /// cursor.
    fn start_pull(&mut self) {
        let Some(name) = self.active_worktree_name() else {
            return;
        };
        let then = BusyThen::Pull { name: name.clone() };
        self.start_busy(format!("pulling {name}…"), then, move |ctx| {
            ops::pull(ctx, &name, false)
                .map(|r| {
                    if r.already_up_to_date {
                        format!("'{}' already up to date", r.name)
                    } else {
                        format!("pulled '{}'", r.name)
                    }
                })
                .map_err(|e| format!("{e:#}"))
        });
    }

    /// Retries a refused fast-forward pull with a rebase, from the
    /// `ConfirmPullRebase` prompt.
    fn start_pull_rebase(&mut self, name: String) {
        // A rebasing pull can stop on a conflict, which leaves the worktree
        // mid-rebase rather than failing. Route through `Resolve` so that lands
        // in the resolver instead of an error popup with no way forward.
        let then = BusyThen::Resolve {
            target: name.clone(),
            source_label: "its upstream".to_string(),
            kind: ops::ResolveKind::Rebase,
        };
        self.start_busy(
            format!("rebasing {name} onto its upstream…"),
            then,
            move |ctx| {
                ops::pull(ctx, &name, true)
                    .map(|r| format!("pulled '{}' with rebase", r.name))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Pushes the selected worktree (auto-publishing when it has no upstream).
    /// On the Changes tab this is the worktree whose diffs are open.
    fn start_push(&mut self) {
        let Some(name) = self.active_worktree_name() else {
            return;
        };
        self.start_busy(format!("pushing {name}…"), BusyThen::List, move |ctx| {
            ops::push(ctx, &name, false)
                .map(|r| {
                    if r.set_upstream {
                        format!(
                            "pushed '{}' and set upstream {}/{}",
                            r.name,
                            r.remote.as_deref().unwrap_or("origin"),
                            r.branch
                        )
                    } else {
                        format!("pushed '{}'", r.name)
                    }
                })
                .map_err(|e| format!("{e:#}"))
        });
    }

    /// Worktree name for pull/push: Changes-tab target when that tab is open,
    /// otherwise the Worktrees-tab selection.
    fn active_worktree_name(&self) -> Option<String> {
        if self.tab == Tab::Changes && !self.changes.name.is_empty() {
            return Some(self.changes.name.clone());
        }
        self.selected_worktree().map(|w| w.name.clone())
    }

    /// Fast-forwards the branch selected on the Branches tab to its upstream.
    /// Reports the worktree it happened in when the branch is checked out.
    fn start_branch_pull(&mut self) {
        let Some(branch) = self
            .branches
            .get(self.branch_selected)
            .map(|b| b.name.clone())
        else {
            return;
        };
        self.start_busy(
            format!("pulling {branch}…"),
            BusyThen::Branch,
            move |ctx| {
                ops::branch_pull(ctx, &branch)
                    .map(|r| match (r.already_up_to_date, r.worktree) {
                        (true, _) => format!("'{}' already up to date", r.branch),
                        (false, Some(wt)) => format!("fast-forwarded '{}' in {wt}", r.branch),
                        (false, None) => format!("fast-forwarded '{}'", r.branch),
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Fetches all remotes (with prune) in the background, reopening whichever
    /// tab asked so its ahead/behind counts reload.
    fn start_fetch(&mut self) {
        let then = match self.tab {
            Tab::Worktrees | Tab::Changes | Tab::Stash | Tab::Settings => BusyThen::List,
            Tab::Branches => BusyThen::Branch,
        };
        self.start_busy("fetching all remotes…".to_string(), then, move |ctx| {
            ops::fetch(ctx)
                .map(|r| {
                    if r.remotes.is_empty() {
                        "no remotes to fetch".to_string()
                    } else {
                        format!("fetched: {}", r.remotes.join(", "))
                    }
                })
                .map_err(|e| format!("{e:#}"))
        });
    }

    /// Starts the delete flow once the delete confirmation resolves. A dirty
    /// worktree first routes through the Stash / Discard prompt; a clean one
    /// proceeds straight to removal. `cached_dirty` is the count captured when
    /// the list loaded, used only as a fallback for the live dirtiness check.
    fn begin_delete(
        &mut self,
        name: String,
        cached_dirty: usize,
        branch: Option<String>,
        delete_branch: bool,
    ) {
        // Re-check dirtiness live rather than trusting the count captured when
        // the list was loaded, since the worktree may have changed since then.
        let dirty = ops::worktree_is_dirty(&self.ctx, &name).unwrap_or(cached_dirty > 0);
        if dirty {
            self.open_delete_dirty_modal(name, branch, delete_branch);
        } else {
            self.do_delete(name, branch, delete_branch, false);
        }
    }

    /// Removes the worktree folder and, when requested, deletes its branch. A
    /// folder-only removal is backgrounded through the Busy overlay; a branch
    /// delete runs synchronously so an unmerged or checked-out-elsewhere
    /// refusal can open the force prompt instead of failing silently.
    fn do_delete(
        &mut self,
        name: String,
        branch: Option<String>,
        delete_branch: bool,
        force: bool,
    ) {
        match (delete_branch, branch) {
            // Remove the folder in the background (the slow part), then delete
            // the branch on the main thread once it lands (see the DeleteBranch
            // follow-up in tick), so an unmerged or checked-out-elsewhere
            // refusal can still open the force prompt. Backgrounding keeps the
            // spinner moving instead of freezing the UI while git works.
            (true, Some(branch)) => {
                let thread_name = name.clone();
                self.start_busy(
                    format!("removing '{name}' and branch '{branch}'…"),
                    BusyThen::DeleteBranch {
                        name: name.clone(),
                        branch,
                    },
                    move |ctx| {
                        ops::remove_worktree_only(ctx, &thread_name, force)
                            .map(|_| String::new())
                            .map_err(|e| format!("{e:#}"))
                    },
                );
            }
            // Folder-only removal (branch kept, or a detached worktree).
            _ => {
                let thread_name = name.clone();
                self.start_busy(format!("removing '{name}'…"), BusyThen::List, move |ctx| {
                    ops::remove_worktree_only(ctx, &thread_name, force)
                        .map(|info| match &info.branch {
                            Some(_) => format!("removed '{}' (branch kept)", info.name),
                            None => format!("removed '{}'", info.name),
                        })
                        .map_err(|e| format!("{e:#}"))
                });
            }
        }
    }

    /// After the folder is removed, attempts a safe branch delete and routes to
    /// the matching force prompt when git refuses.
    fn delete_branch_step(&mut self, name: String, branch: String) {
        match ops::try_delete_branch(&self.ctx, &branch) {
            Ok(ops::DeleteBranchOutcome::Deleted) => {
                self.message = Some(format!("removed '{name}' and branch '{branch}'"));
                self.go_root();
                self.refresh();
            }
            Ok(ops::DeleteBranchOutcome::NotMerged) => {
                // Refresh so the now-removed folder drops from the list behind
                // the popup.
                self.refresh();
                self.open_force_branch_modal(branch, ForceBranchReason::NotMerged);
            }
            Ok(ops::DeleteBranchOutcome::CheckedOutElsewhere(other)) => {
                self.refresh();
                self.open_force_branch_modal(branch, ForceBranchReason::CheckedOutElsewhere(other));
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.go_root();
                self.refresh();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    /// Builds a real single-commit git repo so App can list worktrees.
    /// `initialized` decides whether a `.wtm.toml` exists, i.e. whether the
    /// app opens the list or the setup wizard.
    fn build_app(initialized: bool) -> (tempfile::TempDir, App) {
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
            assert!(out.status.success());
        }
        if initialized {
            std::fs::write(repo.join(".wtm.toml"), "").unwrap();
        }
        // Build the Ctx by hand with a default config so the developer's own
        // global wtm config can't leak into the test.
        let ctx = Ctx {
            repo_root: crate::git::repo_root(&repo).unwrap(),
            config: crate::config::Config::default(),
        };
        let mut app = App::new(ctx).unwrap();
        // Same reasoning for the Settings tab's global-config target: point it
        // inside the temp dir so saving can never rewrite the developer's own
        // ~/.config/wtm/config.toml.
        app.settings.global_config = Some(tmp.path().join("global.toml"));
        (tmp, app)
    }

    /// Just the templates of a command list, for asserting on what is
    /// configured without spelling out each entry's mode and scope.
    fn command_texts(commands: &[OpenCommand]) -> Vec<String> {
        commands.iter().map(|c| c.command.clone()).collect()
    }

    fn test_app() -> (tempfile::TempDir, App) {
        build_app(true)
    }

    fn test_app_uninitialized() -> (tempfile::TempDir, App) {
        build_app(false)
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// Drives `tick` until an in-flight background op lands, the way the event
    /// loop does, so the test can assert on its result.
    fn settle_busy(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(app.view, View::Busy { .. }) {
            app.tick();
            assert!(std::time::Instant::now() < deadline, "busy op timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A `LogEntry` with only its hash set, for tests that care about row
    /// structure rather than commit contents.
    fn log_entry(hash: &str) -> LogEntry {
        LogEntry {
            hash: hash.to_string(),
            subject: String::new(),
            author: String::new(),
            date: String::new(),
            refs: Vec::new(),
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
        // Drain background loads the event loop's tick would pick up, so tests
        // that assert right after a key don't race in-flight status/stash/log.
        settle_background(app);
    }

    /// Polls every non-busy background load until idle (status, commits, stash).
    fn settle_background(app: &mut App) {
        settle_status_refresh(app);
        settle_commits(app);
        settle_stash(app);
    }

    /// Opens the Settings tab the same way the tab bar / `select_tab` does.
    fn goto_settings(app: &mut App) {
        app.select_tab(Tab::Settings);
    }

    /// Switches tabs and drains any background load that entry kicked off
    /// (status, stash, commits, branches), matching what a real frame would see.
    fn goto_tab(app: &mut App, tab: Tab) {
        app.select_tab(tab);
        settle_background(app);
        if tab == Tab::Branches {
            settle_branches(app);
        }
    }

    /// Waits out an in-flight background branch-list load (item 4 made
    /// `load_branches` async), so tests can assert on `app.branches` right
    /// after switching to the Branches tab.
    fn settle_branches(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.branches_loading() {
            app.poll_branches_load();
            assert!(
                std::time::Instant::now() < deadline,
                "branch load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits out an in-flight Worktrees-tab preview status load so tests can
    /// assert on `app.worktree_preview` after a selection change or refresh.
    /// Also drains a follow-on three-panel commits load and any status refresh
    /// kicked off by syncing the changes pane.
    fn settle_preview(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.ensure_worktree_preview();
            app.poll_preview_load();
            app.poll_commits_load();
            app.poll_status_refresh();
            if app.preview_for == Some(app.selected)
                && app.commits_pending.is_none()
                && app.status_refresh_pending.is_none()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worktree preview load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits out an in-flight `refresh_diff` / `load_changes` status load.
    fn settle_status_refresh(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.status_refresh_pending.is_some() {
            app.poll_status_refresh();
            app.poll_commits_load();
            assert!(
                std::time::Instant::now() < deadline,
                "status refresh timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits out an in-flight three-panel commits-panel log load.
    fn settle_commits(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.commits_pending.is_some() {
            app.poll_commits_load();
            assert!(
                std::time::Instant::now() < deadline,
                "commits panel load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits out an in-flight Stash tab list load.
    fn settle_stash(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.stash_loading() {
            app.poll_stash_load();
            assert!(
                std::time::Instant::now() < deadline,
                "stash list load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Drains an in-flight `View::Busy` op the way the event loop does, so tests
    /// can assert on the settled state after a backgrounded action.
    fn settle(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(app.view, View::Busy { .. }) {
            app.tick();
            assert!(std::time::Instant::now() < deadline, "busy op timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Busy follow-ups (stash reload, refresh_diff) start more background
        // work; drain those too before the caller asserts.
        settle_background(app);
    }

    fn press_shift(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::SHIFT));
        settle_background(app);
    }

    /// Waits out an in-flight background diff load (item 1 made file diffs
    /// async), so tests can assert on `content` right after navigating.
    fn settle_diff(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.changes.pending.is_some() {
            app.poll_diff_load();
            assert!(std::time::Instant::now() < deadline, "diff load timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Waits out an in-flight diff load in the commit/stash file browser, so
    /// tests can assert on the `content` the background thread produced.
    fn settle_browser_diff(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(
            app.view,
            View::CommitDiff {
                pending: Some(_),
                ..
            } | View::StashDiff {
                pending: Some(_),
                ..
            }
        ) {
            app.poll_file_browser_diff_load();
            assert!(std::time::Instant::now() < deadline, "diff load timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Branch names the switch picker currently offers under its filter, in
    /// display order. Panics unless the picker is open.
    fn switch_matches(app: &App) -> Vec<String> {
        let View::Switch {
            branches, filter, ..
        } = &app.view
        else {
            panic!("expected the switch picker");
        };
        filtered_candidates(branches, filter.as_str())
            .into_iter()
            .map(|i| branches[i].branch.clone())
            .collect()
    }

    fn scroll_wheel(app: &mut App, kind: MouseEventKind) {
        app.on_mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });
    }

    fn click(app: &mut App, col: u16, row: u16) {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        });
        settle_background(app);
    }

    fn ctrl_c(app: &mut App) {
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    /// Moves the Changes tab's cursor onto the row for the file named `path`,
    /// panicking if it isn't in the list. Skips over folder rows.
    fn select_diff_file(app: &mut App, path: &str) {
        assert_eq!(app.tab, Tab::Changes, "expected the Changes tab");
        settle_status_refresh(app);
        loop {
            let c = &app.changes;
            if let Some(i) = current_file_index(&c.rows, c.selected)
                && c.files[i].path == path
            {
                settle_diff(app);
                return;
            }
            assert!(c.selected + 1 < c.rows.len(), "{path} not in the diff list");
            press(app, KeyCode::Down);
        }
    }

    /// Ticks the app until the Creating view satisfies `pred`, panicking
    /// after 10 seconds.
    fn wait_creating(app: &mut App, pred: impl Fn(&[String], bool) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.tick();
            match &app.view {
                View::Creating { lines, done, .. } => {
                    if pred(lines, *done) {
                        return;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting; lines so far: {lines:?}"
                    );
                }
                _ => panic!("expected the creating view"),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn is_new_file_flags_untracked_and_added_codes() {
        assert!(is_new_file("??"));
        assert!(is_new_file("A "));
        assert!(is_new_file("AM"));
        assert!(!is_new_file(" M"));
        assert!(!is_new_file("M "));
        assert!(!is_new_file(" D"));
    }

    #[test]
    fn lists_main_worktree_on_startup() {
        let (_tmp, app) = test_app();
        assert_eq!(app.worktrees.len(), 1);
        assert!(app.worktrees[0].is_main);
    }

    #[test]
    fn q_quits_and_question_mark_opens_help() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('?'));
        assert!(app.show_help);
        // Help opens on the page for the view underneath, not a fixed one.
        assert_eq!(app.help_tab, HelpTab::Worktrees);
        // Any key the panel doesn't use closes it, returning to that view.
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.show_help);
        assert!(matches!(app.view, View::List));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    /// From a nested commit-diff view, two rapid q presses must back out (to the
    /// list), not quit — a second q buffered during a slow frame used to quit.
    #[test]
    fn rapid_q_from_commit_diff_backs_out_without_quitting() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('l'));
        assert!(matches!(app.view, View::Log { .. }));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::CommitDiff { .. }));
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit, "second q after back must not quit the app");
        assert!(
            matches!(app.view, View::List),
            "two q presses from CommitDiff should reach the list"
        );
        // Intentional quit from the Worktrees list still works once the ignore
        // window is cleared (or expires).
        app.ignore_quit_until = None;
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    /// Changes is entered like a drill-in (Enter from Worktrees); q returns
    /// there, and a buffered duplicate must not quit.
    #[test]
    fn rapid_q_from_changes_returns_without_quitting() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.tab, Tab::Changes);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.tab, Tab::Worktrees);
        assert!(!app.quit);
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit, "duplicate q right after back must be ignored");
        app.ignore_quit_until = None;
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn help_opens_on_the_tab_for_the_active_view() {
        let (_tmp, mut app) = test_app();
        // The Branches tab of the list gets the Branches page.
        app.tab = Tab::Branches;
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.help_tab, HelpTab::Branches);
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_help);
        // Views with no page of their own land on Basics.
        app.tab = Tab::Worktrees;
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::F(1));
        assert!(app.show_help);
        assert_eq!(app.help_tab, HelpTab::Basics);
    }

    #[test]
    fn help_tabs_cycle_and_reset_scroll() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.help_tab, HelpTab::Worktrees);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.help_scroll, 1);
        // Switching tabs starts the new page from the top.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.help_tab, HelpTab::Branches);
        assert_eq!(app.help_scroll, 0);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.help_tab, HelpTab::Worktrees);
        // Scrolling up off the top saturates rather than wrapping around.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.help_scroll, 0);
        assert!(app.show_help);
    }

    /// The regression this panel exists for: the old fixed 58-row popup ran off
    /// the bottom of a short terminal and silently dropped its last sections.
    /// Every tab must now fit and stay scrollable at 80x24.
    #[test]
    fn help_fits_and_scrolls_on_a_short_terminal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('?'));
        for _ in 0..HelpTab::ALL.len() {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| super::super::ui::draw(frame, &mut app))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let rows: Vec<String> = (0..24)
                .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            let screen = rows.join("\n");
            assert!(
                screen.contains(app.help_tab.title()),
                "{} not drawn:\n{screen}",
                app.help_tab.title()
            );
            // Scrolling to the bottom must reach the last line of the page.
            for _ in 0..40 {
                press(&mut app, KeyCode::Down);
            }
            press(&mut app, KeyCode::Tab);
        }
        // Six tabs later we are back where we started, still in help.
        assert!(app.show_help);
        assert_eq!(app.help_tab, HelpTab::Worktrees);
    }

    #[test]
    fn f1_opens_help_where_question_mark_is_a_literal() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('n'));
        // The create dialog's name field must receive '?' as text.
        type_str(&mut app, "fix/what?");
        assert!(!app.show_help);
        let View::Create { name, .. } = &app.view else {
            panic!("expected the create dialog");
        };
        assert_eq!(name.value, "fix/what?");
        // F1 is the way into help from a view that is taking input.
        press(&mut app, KeyCode::F(1));
        assert!(app.show_help);
        // Closing help leaves the typed name untouched.
        press(&mut app, KeyCode::Esc);
        let View::Create { name, .. } = &app.view else {
            panic!("expected the create dialog");
        };
        assert_eq!(name.value, "fix/what?");
    }

    /// A long error must be readable, so only q/Esc/Enter close it; every
    /// other key scrolls the message instead of throwing it away unread.
    #[test]
    fn only_quit_keys_dismiss_the_error_popup() {
        let (_tmp, mut app) = test_app();
        app.set_error("boom");
        assert!(app.error.is_some());
        press(&mut app, KeyCode::Char('x'));
        assert!(app.error.is_some(), "a stray key must not close the error");
        press(&mut app, KeyCode::Char('q'));
        assert!(app.error.is_none());

        app.set_error("boom");
        press(&mut app, KeyCode::Esc);
        assert!(app.error.is_none(), "Esc closes it too");
    }

    /// Scrolling is clamped to the message: the renderer records how far it can
    /// go, and the keys never move past either end of that range.
    #[test]
    fn error_popup_scrolls_within_its_message() {
        let (_tmp, mut app) = test_app();
        app.set_error("line one\nline two\nline three");
        assert_eq!(app.error_scroll, 0);
        // Nothing has been drawn yet, so there is no room to scroll.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.error_scroll, 0, "clamped to the recorded maximum");

        app.error_max_scroll = 2;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.error_scroll, 1);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.error_scroll, 2, "stops at the last line");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.error_scroll, 1);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.error_scroll, 0, "stops at the first line");

        // A new error always starts at the top of its own message.
        app.error_scroll = 2;
        app.set_error("something else");
        assert_eq!(app.error_scroll, 0);
    }

    #[test]
    fn create_dialog_name_input_moves_cursor() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "abc");
        // Move the cursor left and insert in the middle.
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('X'));
        match &app.view {
            View::Create { name, .. } => {
                assert_eq!(name.as_str(), "abXc");
                assert_eq!(name.cursor, 3);
            }
            _ => panic!("expected create dialog"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
    }

    /// Sets up `root` (already the checked-out main worktree of `app`) with a
    /// bare `origin` one commit ahead of local `main`: an independent clone
    /// makes and pushes a commit that `root` never fetched. `root`'s `main`
    /// ends up behind its upstream by one commit with nothing of its own
    /// ahead, so it's a clean fast-forward candidate.
    fn make_root_behind_upstream(tmp: &tempfile::TempDir, root: &Path) -> String {
        let bare = tmp.path().join("origin.git");
        git(
            tmp.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(root, &["push", "-u", "origin", "main"]);
        let clone = tmp.path().join("clone");
        git(
            tmp.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.email", "t@e.st"]);
        git(&clone, &["config", "user.name", "t"]);
        git(&clone, &["commit", "--allow-empty", "-m", "remote-work"]);
        git(&clone, &["push", "origin", "main"]);
        // Ahead/behind is computed against the last-known remote-tracking ref
        // (the same as `git status`), so fetch to pick up the new commit —
        // just as a user would before wtm reports the base as behind.
        git(root, &["fetch"]);
        crate::git::run(&clone, &["log", "-1", "--format=%H"]).unwrap()
    }

    /// Creating a new branch off a base that's behind its upstream opens a
    /// prompt; confirming "pull, then create" fast-forwards the base first,
    /// so the new branch starts from the up-to-date tip.
    #[test]
    fn create_dialog_prompts_to_pull_a_behind_base_and_pulls() {
        let (tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        let remote_head = make_root_behind_upstream(&tmp, &root);
        app.refresh();

        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        match &app.modal {
            Some(Modal::Confirm {
                action: ModalAction::ConfirmPullBase { branch, target, .. },
                ..
            }) => {
                assert_eq!(branch, "feature");
                assert_eq!(target, "main");
            }
            _ => panic!("expected a prompt to pull the behind base"),
        }
        press(&mut app, KeyCode::Enter); // "pull, then create"
        assert_eq!(
            crate::git::run(&root, &["rev-parse", "main"]).unwrap(),
            remote_head,
            "main should be fast-forwarded to the remote tip"
        );
        wait_creating(&mut app, |_, done| done);
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == "feature"));
        let feature_path = app
            .worktrees
            .iter()
            .find(|w| w.name == "feature")
            .unwrap()
            .path
            .clone();
        assert_eq!(
            crate::git::run(Path::new(&feature_path), &["rev-parse", "HEAD"]).unwrap(),
            remote_head,
            "the new branch should start from the pulled-in commit"
        );
    }

    /// Choosing "create without pulling" on the behind-base prompt leaves the
    /// base untouched and branches off it as-is.
    #[test]
    fn create_dialog_can_skip_pulling_a_behind_base() {
        let (tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        make_root_behind_upstream(&tmp, &root);
        let local_head = crate::git::run(&root, &["rev-parse", "main"]).unwrap();
        app.refresh();

        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm {
                action: ModalAction::ConfirmPullBase { .. },
                ..
            })
        ));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // "create without pulling"
        assert_eq!(
            crate::git::run(&root, &["rev-parse", "main"]).unwrap(),
            local_head,
            "main should be untouched"
        );
        wait_creating(&mut app, |_, done| done);
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == "feature"));
    }

    #[test]
    fn create_dialog_offers_existing_branches() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        for args in [["branch", "spare"], ["branch", "other"]] {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        press(&mut app, KeyCode::Char('n'));
        match &app.view {
            View::Create {
                branches, selected, ..
            } => {
                // main is checked out, so only the two spare branches show.
                assert_eq!(*selected, 0);
                assert!(branches.iter().any(|c| c.branch == "spare"));
                assert!(branches.iter().any(|c| c.branch == "other"));
                assert!(!branches.iter().any(|c| c.branch == "main"));
            }
            _ => panic!("expected create dialog"),
        }
        // ↓ into the checkout list, then pick the highlighted existing branch.
        press(&mut app, KeyCode::Down);
        let expected = match &app.view {
            View::Create {
                branches, selected, ..
            } => branches[*selected - 1].branch.clone(),
            _ => panic!("expected create dialog"),
        };
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Creating { branch, .. } => assert_eq!(*branch, expected),
            _ => panic!("expected creating view"),
        }
        wait_creating(&mut app, |_, done| done);
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == expected));
    }

    #[test]
    fn create_dialog_new_branch_uses_typed_name() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Creating { branch, .. } => assert_eq!(branch, "feature"),
            _ => panic!("expected creating view"),
        }
        wait_creating(&mut app, |_, done| done);
        match &app.view {
            View::Creating { done, outcome, .. } => {
                assert!(*done);
                // The pinned banner is driven by `outcome`, not by a line
                // pushed into the log that a long setup run could scroll away.
                let outcome = outcome
                    .as_ref()
                    .expect("finished create must report an outcome");
                assert!(outcome.ok, "setup should have succeeded");
                assert!(
                    !outcome.path.is_empty(),
                    "banner must show the worktree path"
                );
                assert!(outcome.detail.is_none());
            }
            _ => panic!("expected creating view"),
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == "feature"));
    }

    #[test]
    fn create_dialog_base_button_focus_and_pick() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "release"]);
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        // Tab focuses the base button; a second Tab opens the base picker.
        press(&mut app, KeyCode::Tab);
        match &app.view {
            View::Create {
                base_focus,
                base_pick,
                ..
            } => {
                assert!(*base_focus);
                assert!(base_pick.is_none());
            }
            _ => panic!("expected create dialog"),
        }
        press(&mut app, KeyCode::Tab);
        assert!(matches!(
            app.view,
            View::Create {
                base_pick: Some(_),
                ..
            }
        ));
        // Point the picker at "release" and confirm it as the base.
        if let View::Create {
            all_branches,
            base_pick,
            ..
        } = &mut app.view
        {
            *base_pick = Some(all_branches.iter().position(|b| b == "release").unwrap());
        }
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Create {
                base, base_pick, ..
            } => {
                assert_eq!(base, "release");
                assert!(base_pick.is_none());
            }
            _ => panic!("expected create dialog"),
        }
    }

    #[test]
    fn tab_key_cycles_top_level_tabs() {
        let (_tmp, mut app) = test_app();
        assert_eq!(app.tab, Tab::Worktrees);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Changes);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        // Entering the Branches tab loads the branch list in the background.
        settle_branches(&mut app);
        assert!(!app.branches.is_empty());
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Stash);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Settings);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Worktrees);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.tab, Tab::Settings);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.tab, Tab::Stash);
    }

    /// Switching to the Branches tab renders immediately (never blocks on the
    /// per-branch git invocations behind `load_branches`): the first visit
    /// shows a loading state until the background load lands, and a later
    /// visit within the cache window reuses the list without kicking off
    /// another load.
    #[test]
    fn switching_to_branches_tab_is_instant_with_loading_indicator() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        assert!(
            app.branches_first_load(),
            "no cached branches yet on the first visit"
        );
        settle_branches(&mut app);
        assert!(!app.branches_first_load());
        assert!(!app.branches.is_empty());

        // Leave and come back within the cache window: no reload, cached list.
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        assert!(
            !app.branches_loading(),
            "a fresh cache must not reload on re-entry"
        );
        assert!(!app.branches.is_empty(), "the cached list stays visible");
        assert!(!app.branches_first_load());
    }

    /// Manual `r` on the Branches tab always reloads, even when the cache is
    /// still within the timeout window.
    #[test]
    fn branches_tab_manual_refresh_reloads_a_fresh_cache() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);
        assert!(!app.branches_loading());

        press(&mut app, KeyCode::Char('r'));
        assert!(
            app.branches_loading(),
            "manual refresh must bypass the cache"
        );
        assert!(
            !app.branches.is_empty(),
            "stale list stays until the load lands"
        );
        settle_branches(&mut app);
    }

    /// Expiring the Branches cache (or invalidating it after a worktree create)
    /// makes the next visit kick off a reload.
    #[test]
    fn branches_tab_reloads_after_cache_invalidation() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);

        press(&mut app, KeyCode::BackTab); // Changes
        app.invalidate_branches_cache();
        press(&mut app, KeyCode::Tab); // Branches again
        assert!(
            app.branches_loading(),
            "an invalidated cache must reload on the next visit"
        );
        settle_branches(&mut app);
    }

    #[test]
    fn switch_with_no_other_branches_still_opens_the_picker() {
        let (_tmp, mut app) = test_app();
        // Only the main branch exists and it is checked out, so the list is
        // empty. The picker still opens: a branch can be typed in by hand.
        press(&mut app, KeyCode::Char('b'));
        assert!(matches!(app.view, View::Switch { .. }));
        assert!(switch_matches(&app).is_empty());
    }

    #[test]
    fn main_worktree_cannot_be_deleted() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.view, View::List));
        assert!(app.message.as_deref().unwrap().contains("main worktree"));
    }

    #[test]
    fn reverting_a_new_file_reports_it_cannot_be_reverted() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Enter);
        // The only change is the untracked `.wtm.toml`, so the cursor sits on a
        // brand-new file. Undo has nothing to restore to.
        press(&mut app, KeyCode::Char('u'));
        assert!(app.modal.is_none(), "undo must not prompt for a new file");
        assert_eq!(app.tab, Tab::Changes);
        let msg = app.message.as_deref().unwrap();
        assert!(msg.contains("new") && msg.contains("delete"), "got: {msg}");
    }

    #[test]
    fn deleting_a_file_from_the_diff_view_removes_it() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Enter);
        // 'd' asks to confirm; 'y' deletes the highlighted file.
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        assert_eq!(app.tab, Tab::Changes);
        press(&mut app, KeyCode::Char('y'));
        assert!(app.message.as_deref().unwrap().contains("deleted"));
        // After deleting the sole change, the Changes tab has no files left.
        assert!(app.changes.files.is_empty());
    }

    #[test]
    fn enter_opens_diff_and_scrolls() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.tab, Tab::Changes);
        assert!(
            !app.changes.files.is_empty(),
            "the untracked .wtm.toml shows"
        );
        // Shift+Down scrolls the diff content; each press moves three lines.
        press_shift(&mut app, KeyCode::Down);
        press_shift(&mut app, KeyCode::Down);
        assert_eq!(app.changes.scroll, 6);
        // Capital J/K scroll on terminals that don't report the Shift modifier
        // on arrow keys; the mouse wheel scrolls too.
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(app.changes.scroll, 9);
        scroll_wheel(&mut app, MouseEventKind::ScrollUp);
        press(&mut app, KeyCode::Char('K'));
        press_shift(&mut app, KeyCode::Up);
        assert_eq!(app.changes.scroll, 0);
        // Esc/q return to the Worktrees list (Enter drills in; quit is from there).
        press(&mut app, KeyCode::Esc);
        assert!(!app.quit);
        assert_eq!(app.tab, Tab::Worktrees);
    }

    #[test]
    fn diff_view_marks_and_reverts_a_file() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        git(&root, &["add", "f.txt"]);
        git(&root, &["commit", "-m", "add f"]);
        std::fs::write(root.join("f.txt"), "two\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "f.txt");
        assert!(
            app.changes.content.contains("two"),
            "shows the file's own diff: {}",
            app.changes.content
        );
        assert!(
            app.changes.marked.iter().all(|m| *m),
            "everything is marked by default"
        );
        // Space unmarks the current file for commit.
        press(&mut app, KeyCode::Char(' '));
        {
            let c = &app.changes;
            let i = c.files.iter().position(|f| f.path == "f.txt").unwrap();
            assert_eq!(current_file_index(&c.rows, c.selected), Some(i));
            assert!(!c.marked[i], "space toggled the mark off");
        }
        // Undo discards the change; f.txt returns to its committed content.
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "one\n"
        );
    }

    #[test]
    fn diff_view_shift_s_stashes_marked_files() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // Two committed files, then edit both so they show as changes.
        for (name, body) in [("a.txt", "a1\n"), ("b.txt", "b1\n")] {
            std::fs::write(root.join(name), body).unwrap();
            git(&root, &["add", name]);
        }
        git(&root, &["commit", "-m", "add ab"]);
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        std::fs::write(root.join("b.txt"), "b2\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter);
        // Unmark b.txt so only a.txt stays marked.
        select_diff_file(&mut app, "b.txt");
        press(&mut app, KeyCode::Char(' '));
        // Shift+S stashes just the marked file (a.txt).
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "a1\n",
            "a.txt was marked, so it was stashed back to committed content"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "b2\n",
            "b.txt was unmarked, so its change is untouched"
        );
    }

    #[test]
    fn diff_view_shift_s_reports_when_nothing_marked() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        git(&root, &["add", "c.txt"]);
        git(&root, &["commit", "-m", "add c"]);
        std::fs::write(root.join("c.txt"), "cc\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter);
        // Unmark all, then Shift+S should refuse rather than stash everything.
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('S'));
        assert!(
            app.message.as_deref().unwrap().contains("no files marked"),
            "message: {:?}",
            app.message
        );
        assert_eq!(
            std::fs::read_to_string(root.join("c.txt")).unwrap(),
            "cc\n",
            "nothing marked, so nothing was stashed"
        );
    }

    #[test]
    fn create_into_existing_worktree_dir_offers_open() {
        let (_tmp, mut app) = test_app();
        // A worktree named "spare" now occupies its target directory.
        add_and_select_worktree(&mut app, "spare");
        app.selected = 0;

        // Typing "spare" as a new branch collides with that directory.
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "spare");
        press(&mut app, KeyCode::Enter);
        match &app.modal {
            Some(Modal::Confirm {
                options,
                selected,
                action: ModalAction::ConfirmExisting { existing_name, .. },
                ..
            }) => {
                assert_eq!(existing_name.as_deref(), Some("spare"));
                assert_eq!(*selected, 0, "defaults to Open for a real worktree");
                assert!(options[0].enabled, "open is enabled for a real worktree");
            }
            _ => panic!("expected the existing-directory prompt"),
        }
        // Enter opens the existing worktree's changes.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.tab, Tab::Changes);
        assert_eq!(app.changes.name, "spare");
    }

    #[test]
    fn row_list_hit_maps_clicks_to_indices() {
        // A list with a one-row header, scrolled down by three rows.
        let rl = RowList {
            inner: Rect::new(2, 5, 20, 4),
            header: 1,
            offset: 3,
            len: 100,
        };
        assert_eq!(rl.hit(3, 5), None, "the header row is not a data row");
        assert_eq!(rl.hit(3, 6), Some(3), "first data row maps to the offset");
        assert_eq!(rl.hit(3, 7), Some(4));
        assert_eq!(rl.hit(3, 8), Some(5), "last visible row");
        assert_eq!(rl.hit(3, 9), None, "below the list");
        assert_eq!(rl.hit(1, 6), None, "left of the list");
        assert_eq!(rl.hit(22, 6), None, "right of the list");

        // A short list: clicks past the last row select nothing.
        let short = RowList {
            inner: Rect::new(0, 0, 10, 10),
            header: 0,
            offset: 0,
            len: 2,
        };
        assert_eq!(short.hit(0, 0), Some(0));
        assert_eq!(short.hit(0, 1), Some(1));
        assert_eq!(short.hit(0, 2), None, "no row there");
    }

    #[test]
    fn diff_view_click_selects_file() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("a.txt"), "1\n").unwrap();
        std::fs::write(root.join("b.txt"), "2\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "add"]);
        std::fs::write(root.join("a.txt"), "11\n").unwrap();
        std::fs::write(root.join("b.txt"), "22\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter); // open diff, cursor on the first row

        // Both files sit at the repo root, so the rows are two file rows with no
        // folder headers. Publish the geometry the renderer would set.
        let len = app.changes.rows.len();
        assert_eq!(len, 2);
        app.row_list = Some(RowList {
            inner: Rect::new(0, 2, 30, 10),
            header: 0,
            offset: 0,
            len,
        });

        // Click the second row (y = inner.y + 1).
        click(&mut app, 1, 3);
        settle_diff(&mut app);
        {
            let c = &app.changes;
            assert_eq!(c.selected, 1, "cursor moved to the clicked row");
            assert_eq!(current_file_index(&c.rows, c.selected), Some(1));
            assert!(
                c.content.contains("22"),
                "clicked file's diff loaded: {}",
                c.content
            );
        }

        // A click outside the list rows leaves the selection untouched.
        click(&mut app, 1, 99);
        assert_eq!(app.changes.selected, 1);
    }

    #[test]
    fn commit_view_click_focuses_and_selects_file() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('c')); // opens the commit view
        assert!(matches!(app.view, View::Commit { .. }));

        let len = match &app.view {
            View::Commit { files, .. } => files.len(),
            _ => panic!("expected commit view"),
        };
        app.row_list = Some(RowList {
            inner: Rect::new(0, 2, 30, 10),
            header: 0,
            offset: 0,
            len: len.min(10),
        });

        click(&mut app, 1, 3); // second file row
        match &app.view {
            View::Commit { cursor, focus, .. } => {
                assert_eq!(*cursor, 1, "cursor moved to the clicked file");
                assert!(
                    matches!(focus, CommitFocus::Files),
                    "focus switched to the file list"
                );
            }
            _ => panic!("expected commit view"),
        }
    }

    #[test]
    fn log_click_moves_the_commit_cursor() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // A couple more commits so a click can move the cursor onto a lower row.
        for (f, m) in [("a.txt", "one"), ("b.txt", "two")] {
            std::fs::write(root.join(f), "x\n").unwrap();
            for args in [vec!["add", f], vec!["commit", "-m", m]] {
                let out = Command::new("git")
                    .args(&args)
                    .current_dir(&root)
                    .output()
                    .unwrap();
                assert!(out.status.success());
            }
        }
        app.selected = 0;
        press(&mut app, KeyCode::Char('l')); // -> Log
        let len = match &app.view {
            View::Log { lines, .. } => lines.len(),
            _ => panic!("expected the log view"),
        };
        assert!(len >= 2, "need at least two commits to test a click");
        app.row_list = Some(RowList {
            inner: Rect::new(0, 2, 40, 10),
            header: 0,
            offset: 0,
            len,
        });
        // Click the second row (y = inner.y + 1). The history is linear, so every
        // row is a selectable commit.
        click(&mut app, 1, 3);
        match &app.view {
            View::Log {
                selected, lines, ..
            } => {
                assert_eq!(*selected, 1, "cursor moved to the clicked commit");
                assert!(lines[*selected].entry.is_some());
            }
            _ => panic!("expected the log view"),
        }
    }

    #[test]
    fn diff_view_i_adds_pattern_to_gitignore() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("debug.log"), "noise\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "debug.log");

        // `i` opens the ignore prompt with the file and its derived pattern.
        press(&mut app, KeyCode::Char('i'));
        match &app.modal {
            Some(Modal::Confirm {
                selected,
                action: ModalAction::IgnorePath { file, pattern },
                ..
            }) => {
                assert_eq!(file, "debug.log");
                assert_eq!(pattern, "*.log");
                assert_eq!(*selected, 0);
            }
            _ => panic!("expected the ignore prompt to be open"),
        }

        // ↓ selects the pattern option; Enter writes it and closes the prompt.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|l| l == "*.log"),
            "pattern written: {gitignore}"
        );
        assert!(app.modal.is_none(), "prompt closed after confirming");
        assert_eq!(app.tab, Tab::Changes);
    }

    #[test]
    fn diff_view_i_can_ignore_single_file_and_esc_cancels() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("secret.log"), "noise\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "secret.log");

        // Esc dismisses the prompt without writing anything.
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Esc);
        assert!(!root.join(".gitignore").exists(), "esc wrote nothing");
        assert!(app.modal.is_none());
        assert_eq!(app.tab, Tab::Changes);

        // Default selection (0) ignores just the file itself.
        select_diff_file(&mut app, "secret.log");
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Enter);
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|l| l == "secret.log"),
            "exact file written: {gitignore}"
        );
    }

    #[test]
    fn commit_from_diff_esc_returns_to_the_changes_tab_it_opened_from() {
        // Cancelling a commit reached from the Changes tab lands back on that
        // tab: `push_screen`/`pop_screen` leave `app.tab` alone.
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("change.txt"), "hi\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Enter); // Worktrees tab -> Changes tab
        assert_eq!(app.tab, Tab::Changes);
        press(&mut app, KeyCode::Char('c')); // Changes -> Commit
        assert!(matches!(app.view, View::Commit { .. }));

        press(&mut app, KeyCode::Esc); // pop back to the Changes tab
        assert!(matches!(app.view, View::List));
        assert_eq!(
            app.tab,
            Tab::Changes,
            "commit cancel returns to the tab it was opened from"
        );
        assert!(app.stack.is_empty(), "back at the root with an empty stack");
    }

    #[test]
    fn commit_from_list_esc_returns_to_the_list() {
        // The same Commit screen reached straight from the list returns to the
        // list, since that is what pushed it.
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("change.txt"), "hi\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Char('c')); // List -> Commit
        assert!(matches!(app.view, View::Commit { .. }));
        press(&mut app, KeyCode::Esc); // pop -> List
        assert!(matches!(app.view, View::List));
        assert!(app.stack.is_empty());
    }

    #[test]
    fn commit_browser_esc_returns_to_the_log_then_the_list() {
        // The read-only commit browser returns to whichever log opened it with
        // no back-reference: popping the stack reveals the log underneath.
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("file.txt"), "one\n").unwrap();
        for args in [vec!["add", "file.txt"], vec!["commit", "-m", "add file"]] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        app.selected = 0;

        press(&mut app, KeyCode::Char('l')); // List -> Log
        assert!(matches!(app.view, View::Log { .. }));
        press(&mut app, KeyCode::Enter); // Log -> CommitDiff
        assert!(matches!(app.view, View::CommitDiff { .. }));

        press(&mut app, KeyCode::Esc); // pop -> Log
        assert!(
            matches!(app.view, View::Log { .. }),
            "commit browser returns to the log it was opened from"
        );
        press(&mut app, KeyCode::Esc); // pop -> List
        assert!(matches!(app.view, View::List));
        assert!(app.stack.is_empty());
    }

    #[test]
    fn diff_view_refreshes_on_r_and_on_tick() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // Commit a tracked file so `git diff HEAD` reflects later edits.
        std::fs::write(root.join("file.txt"), "one\n").unwrap();
        for args in [vec!["add", "file.txt"], vec!["commit", "-m", "add file"]] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success());
        }

        // Edit the tracked file so it shows up as a changed file.
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        app.selected = 0; // main worktree
        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "file.txt");
        assert!(
            app.changes.content.contains("two"),
            "{}",
            app.changes.content
        );

        // A further outside edit is picked up when the user presses `r`.
        std::fs::write(root.join("file.txt"), "three\n").unwrap();
        press(&mut app, KeyCode::Char('r'));
        select_diff_file(&mut app, "file.txt");
        assert!(
            app.changes.content.contains("three"),
            "{}",
            app.changes.content
        );

        // A further edit is picked up by tick once the throttle window passes.
        std::fs::write(root.join("file.txt"), "four\n").unwrap();
        app.changes.last_refresh = Instant::now()
            .checked_sub(DIFF_REFRESH_INTERVAL * 2)
            .unwrap();
        app.tick();
        settle_status_refresh(&mut app);
        select_diff_file(&mut app, "file.txt");
        assert!(
            app.changes.content.contains("four"),
            "{}",
            app.changes.content
        );
    }

    #[test]
    fn auto_refresh_keeps_scroll_on_the_same_file() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // A tracked file with enough lines to scroll through.
        let body: String = (0..40).map(|n| format!("line {n}\n")).collect();
        std::fs::write(root.join("file.txt"), &body).unwrap();
        git(&root, &["add", "file.txt"]);
        git(&root, &["commit", "-m", "add"]);
        std::fs::write(root.join("file.txt"), format!("{body}changed\n")).unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "file.txt");

        // Scroll down, then force the throttled auto-refresh to fire.
        press_shift(&mut app, KeyCode::Down);
        press_shift(&mut app, KeyCode::Down);
        let before = app.changes.scroll;
        assert_eq!(before, 6);
        app.changes.last_refresh = Instant::now()
            .checked_sub(DIFF_REFRESH_INTERVAL * 2)
            .unwrap();
        app.tick();
        settle_status_refresh(&mut app);
        assert_eq!(
            app.changes.scroll, before,
            "auto-refresh must not reset scroll"
        );
    }

    /// Moves the Changes tab's cursor onto the folder row whose prefix is
    /// `prefix`, panicking if it isn't in the list.
    fn select_diff_folder(app: &mut App, prefix: &str) {
        assert_eq!(app.tab, Tab::Changes, "expected the Changes tab");
        loop {
            let c = &app.changes;
            if let Some(DiffRow::Folder { prefix: p, .. }) = c.rows.get(c.selected)
                && p == prefix
            {
                return;
            }
            assert!(
                c.selected + 1 < c.rows.len(),
                "{prefix} not in the diff list"
            );
            press(app, KeyCode::Down);
        }
    }

    #[test]
    fn build_diff_rows_groups_files_into_a_folder_tree() {
        let files = vec![
            StatusEntry {
                code: " M".into(),
                path: "src/tui/app.rs".into(),
            },
            StatusEntry {
                code: " M".into(),
                path: "src/tui/ui.rs".into(),
            },
            StatusEntry {
                code: " M".into(),
                path: "README.md".into(),
            },
        ];
        let rows = build_diff_rows(&files, &HashSet::new());
        // Sorted by path: README.md, then the src/ and src/tui/ folders, then
        // their two files.
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                DiffRow::Folder { prefix, depth, .. } => format!("D{depth}:{prefix}"),
                DiffRow::File { index, depth, .. } => format!("F{depth}:{}", files[*index].path),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "F0:README.md",
                "D0:src/",
                "D1:src/tui/",
                "F2:src/tui/app.rs",
                "F2:src/tui/ui.rs",
            ]
        );
    }

    #[test]
    fn diff_view_space_toggles_a_whole_folder() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/a.txt"), "a\n").unwrap();
        std::fs::write(root.join("pkg/b.txt"), "b\n").unwrap();
        std::fs::write(root.join("top.txt"), "t\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);

        // Space on the pkg/ folder row clears the marks for both files under it
        // while leaving top.txt marked.
        select_diff_folder(&mut app, "pkg/");
        press(&mut app, KeyCode::Char(' '));
        for (f, m) in app.changes.files.iter().zip(app.changes.marked.iter()) {
            if f.path.starts_with("pkg/") {
                assert!(!m, "{} should be unmarked", f.path);
            } else {
                assert!(m, "{} should stay marked", f.path);
            }
        }

        // Space again re-marks the whole folder.
        select_diff_folder(&mut app, "pkg/");
        press(&mut app, KeyCode::Char(' '));
        assert!(app.changes.marked.iter().all(|m| *m));
    }

    #[test]
    fn diff_view_i_ignores_a_whole_folder() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("build/out")).unwrap();
        std::fs::write(root.join("build/out/x.o"), "o\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);

        select_diff_folder(&mut app, "build/");
        // The prompt offers the exact folder path or a bare-name glob.
        press(&mut app, KeyCode::Char('i'));
        match &app.modal {
            Some(Modal::Confirm {
                action: ModalAction::IgnorePath { file, pattern },
                ..
            }) => {
                assert_eq!(file, "build/");
                assert_eq!(pattern, "build/");
            }
            _ => panic!("expected the ignore prompt"),
        }
        // Enter writes the exact folder path.
        press(&mut app, KeyCode::Enter);
        let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|l| l == "build/"),
            "folder written: {gitignore}"
        );
    }

    #[test]
    fn diff_refresh_clamps_scroll_when_content_shrinks() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("file.txt"), "a\nb\nc\n").unwrap();
        for args in [vec!["add", "file.txt"], vec!["commit", "-m", "add"]] {
            Command::new("git")
                .args(&args)
                .current_dir(&root)
                .output()
                .unwrap();
        }
        // Create a multi-line diff, scroll down, then remove the change.
        std::fs::write(root.join("file.txt"), "a\nB\nC\nD\n").unwrap();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "file.txt");
        press_shift(&mut app, KeyCode::Down); // scroll the diff down
        std::fs::write(root.join("file.txt"), "a\nb\nc\n").unwrap();
        press(&mut app, KeyCode::Char('r'));
        // file.txt is clean again and drops out of the list; the reload resets
        // the scroll to the top for whatever file is now selected.
        assert!(
            !app.changes.files.iter().any(|f| f.path == "file.txt"),
            "clean file leaves the changes list"
        );
        assert_eq!(app.changes.scroll, 0, "reload resets the scroll");
    }

    #[test]
    fn uninitialized_repo_opens_setup_wizard_and_esc_quits() {
        let (_tmp, mut app) = test_app_uninitialized();
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(
                    wizard.step,
                    super::setup::Step::Welcome { selected: 0 }
                ));
                assert_eq!(wizard.progress(), "welcome");
            }
            _ => panic!("expected the setup wizard"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }

    #[test]
    fn setup_manual_flow_writes_config_and_enters_list() {
        let (_tmp, mut app) = test_app_uninitialized();
        // Take the "set up this repo" route, pick "inside" (second preset), copy
        // .env, no commands, then confirm on the review screen.
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        // The copy question arrives pre-filled from the repo (nothing detected in
        // the test repo), so the answer is typed from scratch.
        type_str(&mut app, ".env");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter); // blank command list -> review
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::Review { .. }));
                assert_eq!(wizard.draft.worktree_dir, "inside");
                assert_eq!(wizard.draft.copy, vec![".env"]);
                assert_eq!(wizard.progress(), "step 4 of 4");
            }
            _ => panic!("expected the review step"),
        }
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter); // write row

        assert!(matches!(app.view, View::List), "message: {:?}", app.message);
        let file = app.ctx.repo_root.join(".wtm.toml");
        assert!(file.exists());
        assert_eq!(app.ctx.config.worktree_dir.as_deref(), Some("inside"));
        assert_eq!(app.worktrees.len(), 1);
    }

    #[test]
    fn setup_clone_flow_loads_edits_and_writes() {
        let (tmp, mut app) = test_app_uninitialized();
        let source = tmp.path().join("other");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join(".wtm.toml"),
            "worktree_dir = \"home\"\n[setup]\ncopy = [\".env\"]\n",
        )
        .unwrap();

        // Clone route -> type the source repo path -> review shows the draft.
        press(&mut app, KeyCode::Char('2'));
        type_str(&mut app, source.to_str().unwrap());
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::Review { .. }));
                assert_eq!(wizard.draft.worktree_dir, "home");
                assert_eq!(wizard.draft.copy, vec![".env"]);
                // The clone route is two screens, not four.
                assert!(wizard.cloned);
                assert_eq!(wizard.progress(), "step 2 of 2");
            }
            _ => panic!("expected the review step, message: {:?}", app.message),
        }

        // Edit worktree_dir: clear "home", type "inside", save.
        press(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "inside");
        press(&mut app, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);

        assert!(matches!(app.view, View::List), "message: {:?}", app.message);
        let text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(text.contains("worktree_dir = \"inside\""), "{text}");
        assert!(text.contains(".env"), "{text}");
    }

    /// The questions arrive pre-answered from what's in the repo: a `.env` on
    /// disk for the copy list, `package-lock.json` for the setup command. Both
    /// are just pre-filled text the user can edit or clear.
    #[test]
    fn setup_prefills_answers_detected_in_the_repo() {
        let (_tmp, mut app) = test_app_uninitialized();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join(".env"), "TOKEN=1\n").unwrap();
        std::fs::write(root.join("package-lock.json"), "{}\n").unwrap();

        press(&mut app, KeyCode::Enter); // set this repo up
        press(&mut app, KeyCode::Enter); // first location preset
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::CopyFiles { input } => assert_eq!(input.as_str(), ".env"),
                _ => panic!("expected the copy-files question"),
            },
            _ => panic!("expected the wizard"),
        }
        press(&mut app, KeyCode::Enter); // accept the detected copy list
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::RunCommands { commands, input } => {
                    assert!(commands.is_empty(), "the only suggestion sits in the input");
                    assert_eq!(input.as_str(), "npm install");
                }
                _ => panic!("expected the commands question"),
            },
            _ => panic!("expected the wizard"),
        }
        press(&mut app, KeyCode::Enter); // accept `npm install`
        press(&mut app, KeyCode::Enter); // blank line -> review
        match &app.view {
            View::Setup(wizard) => {
                assert_eq!(wizard.draft.copy, vec![".env"]);
                assert_eq!(wizard.draft.run, vec!["npm install"]);
            }
            _ => panic!("expected the review step"),
        }
    }

    /// Backspace on a blank command line takes the previous command back into
    /// the input, so a typo is fixable without leaving the screen.
    #[test]
    fn setup_backspace_takes_back_the_last_command() {
        let (_tmp, mut app) = test_app_uninitialized();
        press(&mut app, KeyCode::Enter); // set this repo up
        press(&mut app, KeyCode::Enter); // first location preset
        press(&mut app, KeyCode::Enter); // no files to copy
        type_str(&mut app, "make buidl");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Backspace);
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::RunCommands { commands, input } => {
                    assert!(commands.is_empty());
                    assert_eq!(input.as_str(), "make buidl");
                }
                _ => panic!("expected the commands question"),
            },
            _ => panic!("expected the wizard"),
        }
        // Backspace with nothing left is harmless.
        for _ in 0..20 {
            press(&mut app, KeyCode::Backspace);
        }
        match &app.view {
            View::Setup(wizard) => assert!(matches!(
                wizard.step,
                super::setup::Step::RunCommands { .. }
            )),
            _ => panic!("expected the wizard"),
        }
    }

    /// Esc steps back one screen the whole way through, keeping the answers
    /// already given rather than restarting the wizard.
    #[test]
    fn setup_esc_walks_back_through_the_questions() {
        let (_tmp, mut app) = test_app_uninitialized();
        press(&mut app, KeyCode::Enter); // set this repo up
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // "inside"
        type_str(&mut app, ".env");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "make");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter); // blank line -> review

        // Review -> commands, with the answer intact.
        press(&mut app, KeyCode::Esc);
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::RunCommands { commands, input } => {
                    assert_eq!(commands, &vec!["make".to_string()]);
                    assert_eq!(input.as_str(), "", "no suggestion is re-added");
                }
                other => panic!("expected the commands question, got {:?}", other.name()),
            },
            _ => panic!("expected the wizard"),
        }
        // Commands -> copy files, still holding what was typed.
        press(&mut app, KeyCode::Esc);
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::CopyFiles { input } => assert_eq!(input.as_str(), ".env"),
                other => panic!("expected the copy question, got {:?}", other.name()),
            },
            _ => panic!("expected the wizard"),
        }
        // Copy files -> location -> welcome, where Esc quits.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::Welcome { .. }))
            }
            _ => panic!("expected the wizard"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }

    /// An emptied copy list stays empty when stepping back onto the question;
    /// the repo's suggestions only fill a question that hasn't been answered.
    #[test]
    fn setup_does_not_re_suggest_a_cleared_answer() {
        let (_tmp, mut app) = test_app_uninitialized();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join(".env"), "TOKEN=1\n").unwrap();

        press(&mut app, KeyCode::Enter); // set this repo up
        press(&mut app, KeyCode::Enter); // first location preset
        // Clear the suggested ".env" and move on with nothing.
        for _ in 0..8 {
            press(&mut app, KeyCode::Backspace);
        }
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc); // back onto the copy question
        match &app.view {
            View::Setup(wizard) => match &wizard.step {
                super::setup::Step::CopyFiles { input } => {
                    assert_eq!(input.as_str(), "", "the cleared answer is respected")
                }
                _ => panic!("expected the copy question"),
            },
            _ => panic!("expected the wizard"),
        }
    }

    /// Clicking a welcome option selects it, and the clone route can be reached
    /// entirely by mouse.
    #[test]
    fn setup_welcome_options_are_clickable() {
        let (_tmp, mut app) = test_app_uninitialized();
        render_app(&mut app, 100, 30);
        let rl = app.row_list.expect("the welcome menu records its geometry");
        click(&mut app, rl.inner.x + 1, rl.inner.y + 1);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(
                    wizard.step,
                    super::setup::Step::Welcome { selected: 1 }
                ));
            }
            _ => panic!("expected the wizard"),
        }
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::ClonePath { .. }));
                assert_eq!(wizard.progress(), "step 1 of 2");
            }
            _ => panic!("expected the clone path input"),
        }
    }

    #[test]
    fn setup_bad_clone_path_stays_on_input_with_error() {
        let (_tmp, mut app) = test_app_uninitialized();
        press(&mut app, KeyCode::Char('2')); // clone route
        type_str(&mut app, "/definitely/not/there");
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::ClonePath { .. }));
            }
            _ => panic!("expected to stay on the path input"),
        }
        assert!(app.message.as_deref().unwrap().contains("does not exist"));
    }

    #[test]
    fn setup_file_browser_picks_a_config() {
        let (tmp, mut app) = test_app_uninitialized();
        let source = tmp.path().join("other");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".wtm.toml"), "worktree_dir = \"home\"\n").unwrap();

        press(&mut app, KeyCode::Char('2')); // clone route -> path input
        press(&mut app, KeyCode::Tab); // open the browser at tmp (repo parent)
        // Entries: dirs first alphabetically -> "other" before "proj".
        press(&mut app, KeyCode::Enter); // descend into other/
        press(&mut app, KeyCode::Enter); // pick .wtm.toml
        match &app.view {
            View::Setup(wizard) => {
                assert!(
                    matches!(wizard.step, super::setup::Step::Review { .. }),
                    "message: {:?}",
                    app.message
                );
                assert_eq!(wizard.draft.worktree_dir, "home");
            }
            _ => panic!("expected the review step"),
        }
    }

    /// Creates a worktree via ops and selects it in the list.
    fn add_and_select_worktree(app: &mut App, branch: &str) {
        ops::create(&app.ctx, branch, None, ops::RunMode::Capture, |_| {}).unwrap();
        app.refresh();
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == branch)
            .expect("new worktree should be listed");
    }

    /// Runs a git command in `dir`, asserting it succeeds.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }

    /// Simulates a teammate's fetched branch: `<remote>/<branch>` pointing at
    /// HEAD, with no local branch of its own. The remote is registered (but
    /// never fetched from), since git only treats the ref as a remote-tracking
    /// branch when its remote is configured.
    fn make_remote_ref(root: &Path, remote: &str, branch: &str) {
        git(
            root,
            &["remote", "add", remote, "https://example.invalid/repo.git"],
        );
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        git(
            root,
            &[
                "update-ref",
                &format!("refs/remotes/{remote}/{branch}"),
                sha.trim(),
            ],
        );
    }

    /// Writes an untracked file into the main worktree so it reads as dirty.
    fn dirty_main(app: &mut App) {
        std::fs::write(app.ctx.repo_root.join("scratch.txt"), "work\n").unwrap();
        app.refresh();
        app.selected = 0;
    }

    #[test]
    fn commit_flow_commits_all_changes() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        assert!(app.worktrees[0].dirty > 0);
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::Commit { .. }));
        type_str(&mut app, "add scratch");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(matches!(app.view, View::List), "message: {:?}", app.message);
        assert!(app.message.as_deref().unwrap().starts_with("committed"));
        app.refresh();
        assert_eq!(app.worktrees[0].dirty, 0, "worktree should be clean now");
    }

    /// Item 7: the commit message field supports mid-string editing with the
    /// arrow keys, not just append/backspace at the end.
    #[test]
    fn commit_message_supports_cursor_editing() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        press(&mut app, KeyCode::Char('c'));
        type_str(&mut app, "fix bug");
        // Move the cursor back over "bug" and insert a word before it.
        for _ in 0..3 {
            press(&mut app, KeyCode::Left);
        }
        type_str(&mut app, "the ");
        match &app.view {
            View::Commit { input, .. } => assert_eq!(input.as_str(), "fix the bug"),
            _ => panic!("expected the commit dialog"),
        }
    }

    #[test]
    fn commit_on_clean_worktree_is_reported() {
        // A freshly created worktree has no untracked files, unlike the main
        // one in tests (which carries an uncommitted .wtm.toml).
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "clean");
        assert_eq!(app.worktrees[app.selected].dirty, 0);
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::List));
        assert!(app.message.as_deref().unwrap().contains("clean"));
    }

    /// Tab reaches the body, where Enter makes a new line instead of
    /// committing, and Ctrl+S writes subject + blank line + body.
    #[test]
    fn commit_body_is_written_below_the_subject() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        let dir = app.worktrees[0].path.clone();
        press(&mut app, KeyCode::Char('c'));
        type_str(&mut app, "add scratch");

        press(&mut app, KeyCode::Tab); // message -> body
        match &app.view {
            View::Commit { focus, .. } => assert_eq!(*focus, CommitFocus::Body),
            _ => panic!("expected the commit dialog"),
        }
        type_str(&mut app, "why it matters");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "second line");
        match &app.view {
            View::Commit { body, .. } => {
                assert!(
                    matches!(app.view, View::Commit { .. }),
                    "Enter must not commit"
                );
                assert_eq!(body.lines, vec!["why it matters", "second line"]);
            }
            _ => panic!("Enter in the body must not commit"),
        }

        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        settle(&mut app);
        assert!(matches!(app.view, View::List), "message: {:?}", app.message);
        let full = String::from_utf8(
            Command::new("git")
                .args(["log", "-1", "--format=%B"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(
            full.trim_end(),
            "add scratch\n\nwhy it matters\nsecond line",
            "git must separate subject and body with a blank line"
        );
    }

    /// An untouched body field commits exactly as before: subject only.
    #[test]
    fn commit_without_a_body_stays_subject_only() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        let dir = app.worktrees[0].path.clone();
        press(&mut app, KeyCode::Char('c'));
        type_str(&mut app, "just a subject");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        let full = String::from_utf8(
            Command::new("git")
                .args(["log", "-1", "--format=%B"])
                .current_dir(&dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(full.trim_end(), "just a subject");
    }

    #[test]
    fn commit_empty_message_is_rejected() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Enter); // empty message
        assert!(matches!(app.view, View::Commit { .. }), "stays open");
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("must not be empty")
        );
    }

    #[test]
    fn stash_push_then_pop_round_trips() {
        let (_tmp, mut app) = test_app();
        // A tracked, modified file so stash has something to save.
        std::fs::write(app.ctx.repo_root.join("f.txt"), "one\n").unwrap();
        git(&app.ctx.repo_root, &["add", "f.txt"]);
        git(&app.ctx.repo_root, &["commit", "-m", "add f"]);
        std::fs::write(app.ctx.repo_root.join("f.txt"), "two\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.tab, Tab::Stash);
        // Stash the current changes with a message.
        press(&mut app, KeyCode::Char('s'));
        type_str(&mut app, "wip");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert_eq!(app.tab, Tab::Stash);
        assert_eq!(app.stash_entries.len(), 1);
        assert!(app.stash_entries[0].message.contains("wip"));
        app.refresh();
        assert_eq!(app.worktrees[0].dirty, 0, "stash should clean the tree");

        // Pop it back: 'p' opens the destination picker (stashes are
        // repo-global), defaulting to the worktree the tab was opened from.
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.view, View::StashTarget { pop: true, .. }));
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert_eq!(app.tab, Tab::Stash);
        assert!(app.stash_entries.is_empty());
        app.refresh();
        assert!(app.worktrees[0].dirty > 0, "pop restores the change");
    }

    /// `a` opens a destination picker (stashes are repo-global, not tied to
    /// one worktree) rather than applying straight into the tab's worktree;
    /// picking a target still applies the stash there.
    #[test]
    fn stash_apply_key_opens_target_picker_and_applies() {
        let (_tmp, mut app) = test_app();
        // Commit f.txt before branching "feature" off it, so both worktrees
        // share the same base and the stash can apply cleanly to either.
        std::fs::write(app.ctx.repo_root.join("f.txt"), "one\n").unwrap();
        git(&app.ctx.repo_root, &["add", "f.txt"]);
        git(&app.ctx.repo_root, &["commit", "-m", "add f"]);
        add_and_select_worktree(&mut app, "feature");
        std::fs::write(app.ctx.repo_root.join("f.txt"), "two\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.tab, Tab::Stash);
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Enter); // stash, no message
        settle(&mut app);
        assert_eq!(app.stash_entries.len(), 1);

        press(&mut app, KeyCode::Char('a'));
        match &app.view {
            View::StashTarget {
                pop,
                targets,
                selected,
                ..
            } => {
                assert!(!pop, "apply should not drop the entry");
                // Defaults to the worktree the tab was opened from (main).
                assert!(targets[*selected].branch.as_deref() != Some("feature"));
            }
            _ => panic!("expected the stash target picker"),
        }
        // Pick the other worktree ("feature") as the apply destination.
        if let View::StashTarget {
            selected, targets, ..
        } = &mut app.view
        {
            *selected = targets.iter().position(|t| t.name == "feature").unwrap();
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);

        assert_eq!(
            app.stash_entries.len(),
            1,
            "apply keeps the entry (unlike pop)"
        );
        let feature_path = PathBuf::from(
            app.worktrees
                .iter()
                .find(|w| w.name == "feature")
                .unwrap()
                .path
                .clone(),
        );
        assert_eq!(
            std::fs::read_to_string(feature_path.join("f.txt")).unwrap(),
            "two\n",
            "the stash should have applied into 'feature', not main"
        );
    }

    /// A pop that conflicts routes through the resolver, and completing it
    /// lands back on the Stash tab with the popped entry gone.
    #[test]
    fn conflicting_stash_pop_resolves_back_onto_the_stash_tab() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("c.txt"), "one\n").unwrap();
        git(&root, &["add", "c.txt"]);
        git(&root, &["commit", "-m", "add c"]);
        std::fs::write(root.join("c.txt"), "stashed\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Enter); // stash, no message
        settle(&mut app);
        assert_eq!(app.stash_entries.len(), 1);

        // Commit a different edit to the same line so the pop has to merge.
        std::fs::write(root.join("c.txt"), "committed\n").unwrap();
        git(&root, &["commit", "-am", "diverge"]);

        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.view, View::StashTarget { pop: true, .. }));
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(
            matches!(app.view, View::ConflictResolver { .. }),
            "conflicting pop opens the resolver"
        );

        press(&mut app, KeyCode::Char('t')); // take the stashed side
        press(&mut app, KeyCode::Char('w')); // stage
        press(&mut app, KeyCode::Char('c')); // complete

        assert!(matches!(app.view, View::List));
        assert_eq!(app.tab, Tab::Stash, "back on the tab the pop started from");
        assert!(
            app.stash_entries.is_empty(),
            "a completed pop drops the stash"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("c.txt")).unwrap(),
            "stashed\n"
        );
    }

    #[test]
    fn stash_drop_needs_confirmation() {
        let (_tmp, mut app) = test_app();
        std::fs::write(app.ctx.repo_root.join("g.txt"), "x\n").unwrap();
        git(&app.ctx.repo_root, &["add", "g.txt"]);
        git(&app.ctx.repo_root, &["commit", "-m", "add g"]);
        std::fs::write(app.ctx.repo_root.join("g.txt"), "y\n").unwrap();
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Enter); // stash, no message
        settle(&mut app);
        press(&mut app, KeyCode::Char('x')); // arm drop
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm {
                action: ModalAction::StashDrop { .. },
                ..
            })
        ));
        assert_eq!(app.tab, Tab::Stash, "the tab stays up behind the confirm");
        press(&mut app, KeyCode::Char('y'));
        settle(&mut app);
        assert!(app.stash_entries.is_empty(), "drop removes the entry");
    }

    #[test]
    fn branches_tab_creates_and_deletes_branches() {
        let (_tmp, mut app) = test_app();
        // Tab switches from the Worktrees tab to the Branches tab.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        // Create a new branch "feature".
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        settle_branches(&mut app);
        assert!(crate::git::branch_exists(&app.ctx.repo_root, "feature"));
        assert!(app.branches.iter().any(|b| b.name == "feature"));
        // Select "feature" and delete it (main is not deletable while checked out).
        app.branch_selected = app
            .branches
            .iter()
            .position(|b| b.name == "feature")
            .unwrap();
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "feature"));
    }

    /// A `BranchListItem` reduced to what the display grouping cares about.
    fn branch_item(name: &str, remote: Option<&str>) -> BranchListItem {
        BranchListItem {
            name: name.to_string(),
            checked_out_path: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            subject: String::new(),
            date: String::new(),
            merged: false,
            remote: remote.map(str::to_string),
            created_from: None,
            changed_from_base: false,
            behind_base: false,
        }
    }

    /// The Branches tab draws two labelled groups, local first. Both groups
    /// only appear when they have branches, so a repo with no remotes looks
    /// exactly as it did before grouping existed.
    #[test]
    fn branch_rows_group_local_before_remote() {
        let branches = [
            branch_item("main", None),
            branch_item("upstream-only", Some("origin/upstream-only")),
            branch_item("feature", None),
        ];
        assert_eq!(
            branch_display_rows(&branches),
            [
                BranchRow::Header("LOCAL BRANCHES"),
                BranchRow::Branch(0),
                BranchRow::Branch(2),
                BranchRow::Header("REMOTE BRANCHES"),
                BranchRow::Branch(1),
            ],
            "locals group first even when the list interleaves them"
        );

        let local_only = [branch_item("main", None)];
        assert_eq!(
            branch_display_rows(&local_only),
            [BranchRow::Header("LOCAL BRANCHES"), BranchRow::Branch(0)],
            "an empty group contributes no heading"
        );
        assert!(branch_display_rows(&[]).is_empty());
    }

    /// Clicks arrive as display rows, so they have to be mapped back through
    /// the grouping; a click on a heading selects nothing.
    #[test]
    fn clicking_a_branch_row_maps_past_the_group_headings() {
        let (_tmp, mut app) = test_app();
        app.branches = vec![
            branch_item("main", None),
            branch_item("published", Some("origin/published")),
        ];
        let rows = branch_display_rows(&app.branches);
        assert_eq!(
            branch_row_of(&rows, 1),
            3,
            "remote row sits under 2 headings"
        );

        app.select_branch_row(3);
        assert_eq!(app.branch_selected, 1);
        app.select_branch_row(2); // the REMOTE BRANCHES heading
        assert_eq!(app.branch_selected, 1, "a heading click changes nothing");
        app.select_branch_row(99);
        assert_eq!(app.branch_selected, 1, "an out-of-range click is ignored");
    }

    /// The upstream picker only offers "stop tracking" when there is tracking
    /// to stop, and typing narrows the remote refs.
    #[test]
    fn upstream_picker_rows_filter_and_offer_unset() {
        let candidates = ["origin/main".to_string(), "origin/feature".to_string()];
        assert_eq!(
            upstream_rows(&candidates, "", true),
            [
                UpstreamRow::Unset,
                UpstreamRow::Candidate(0),
                UpstreamRow::Candidate(1),
            ]
        );
        assert_eq!(
            upstream_rows(&candidates, "", false),
            [UpstreamRow::Candidate(0), UpstreamRow::Candidate(1)],
            "nothing to unset when the branch tracks nothing"
        );
        assert_eq!(
            upstream_rows(&candidates, "FEAT", true),
            [UpstreamRow::Unset, UpstreamRow::Candidate(1)],
            "the filter is case-insensitive and keeps the unset row reachable"
        );
    }

    /// `u` on a remote-only row has no local branch to give an upstream, so it
    /// says so instead of opening a picker that could not apply.
    #[test]
    fn upstream_key_refuses_a_remote_only_branch() {
        let (_tmp, mut app) = test_app();
        app.branches = vec![branch_item("published", Some("origin/published"))];
        app.branch_selected = 0;
        app.open_upstream_pick();
        assert!(matches!(app.view, View::List), "no picker opens");
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("only exists on a remote"),
            "{:?}",
            app.message
        );
    }

    #[test]
    fn branches_tab_d_key_opens_confirm_delete() {
        let (_tmp, mut app) = test_app();
        // Tab switches from the Worktrees tab to the Branches tab.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        // The main branch is listed by default, so `d` has something to target.
        assert!(!app.branches.is_empty());
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm {
                action: ModalAction::BranchDelete { .. },
                ..
            })
        ));
    }

    #[test]
    fn branches_tab_c_opens_prefilled_create() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "spare"]);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        app.branch_selected = app.branches.iter().position(|b| b.name == "spare").unwrap();
        // `c` checks out an existing branch, so the create dialog opens with
        // that branch selected in the checkout list.
        press(&mut app, KeyCode::Char('c'));
        match &app.view {
            View::Create {
                branches, selected, ..
            } => {
                assert!(*selected >= 1);
                assert_eq!(branches[*selected - 1].branch, "spare");
            }
            _ => panic!("expected the create dialog prefilled with the branch"),
        }
    }

    #[test]
    fn branches_tab_enter_opens_commits_and_marks_for_cherry_pick() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        // Enter on a branch drills into its commit history.
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::BranchCommits { lines, .. } => assert!(!lines.is_empty()),
            _ => panic!("expected the branch commits view"),
        }
        // Space marks the commit under the cursor, and `p` opens the
        // cherry-pick worktree picker with it selected (Enter now drills into
        // the commit instead).
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('p'));
        match &app.view {
            View::CherryPick {
                commits, targets, ..
            } => {
                assert_eq!(commits.len(), 1);
                assert!(!targets.is_empty());
            }
            _ => panic!("expected the cherry-pick picker"),
        }
    }

    #[test]
    fn branch_commits_enter_browses_the_commit() {
        // Enter now drills into the highlighted commit (like the worktree log),
        // returning to the commit list on Esc.
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);
        press(&mut app, KeyCode::Enter); // Branches -> BranchCommits
        assert!(matches!(app.view, View::BranchCommits { .. }));
        press(&mut app, KeyCode::Enter); // -> CommitDiff (browse, not cherry-pick)
        assert!(matches!(app.view, View::CommitDiff { .. }));
        press(&mut app, KeyCode::Esc); // pop -> BranchCommits
        assert!(matches!(app.view, View::BranchCommits { .. }));
    }

    /// Builds a main-vs-feature merge conflict and drives the UI into the
    /// conflict resolver, returning the feature worktree's path.
    ///
    /// Each entry of `files` is `(path, base, main version, feature version)`;
    /// the two sides diverge from the base so git has to stop.
    fn conflict_resolver_with(
        app: &mut App,
        files: &[(&str, &str, &str, &str)],
    ) -> std::path::PathBuf {
        for (path, base, _, _) in files {
            std::fs::write(app.ctx.repo_root.join(path), base).unwrap();
        }
        git(&app.ctx.repo_root, &["add", "."]);
        git(&app.ctx.repo_root, &["commit", "-m", "base"]);
        add_and_select_worktree(app, "feature");
        let feat = std::path::PathBuf::from(
            app.worktrees
                .iter()
                .find(|w| w.name == "feature")
                .unwrap()
                .path
                .clone(),
        );
        for (path, _, main_version, _) in files {
            std::fs::write(app.ctx.repo_root.join(path), main_version).unwrap();
        }
        git(&app.ctx.repo_root, &["commit", "-am", "main edit"]);
        for (path, _, _, feature_version) in files {
            std::fs::write(feat.join(path), feature_version).unwrap();
        }
        git(&feat, &["commit", "-am", "feature edit"]);
        // Merge main into the feature worktree through the UI.
        press(app, KeyCode::Tab);
        press(app, KeyCode::Tab);
        settle_branches(app);
        let idx = app
            .branches
            .iter()
            .position(|b| b.name == "main")
            .expect("main branch listed");
        app.branch_selected = idx;
        press(app, KeyCode::Char('m'));
        if let View::MergePick {
            targets, selected, ..
        } = &mut app.view
        {
            *selected = targets.iter().position(|t| t.name == "feature").unwrap();
        }
        press(app, KeyCode::Enter);
        settle(app);
        feat
    }

    /// The common case: one conflicted file with a single hunk.
    fn into_conflict_resolver(app: &mut App) -> std::path::PathBuf {
        conflict_resolver_with(
            app,
            &[(
                "shared.txt",
                "base\n",
                "main version\n",
                "feature version\n",
            )],
        )
    }

    /// Two conflicted files, for testing that state survives moving between
    /// them.
    fn into_conflict_resolver_two_files(app: &mut App) -> std::path::PathBuf {
        conflict_resolver_with(
            app,
            &[
                ("a.txt", "base\n", "main a\n", "feature a\n"),
                ("b.txt", "base\n", "main b\n", "feature b\n"),
            ],
        )
    }

    /// One file with two conflict hunks, separated by enough untouched context
    /// that git keeps them apart. Used to test partial resolution.
    fn into_conflict_resolver_multi_hunk(app: &mut App) -> std::path::PathBuf {
        conflict_resolver_with(
            app,
            &[(
                "multi.txt",
                "top\np1\np2\np3\np4\np5\np6\np7\np8\nbottom\n",
                "MAIN top\np1\np2\np3\np4\np5\np6\np7\np8\nMAIN bottom\n",
                "FEAT top\np1\np2\np3\np4\np5\np6\np7\np8\nFEAT bottom\n",
            )],
        )
    }

    #[test]
    fn merge_key_opens_picker_with_worktree_targets() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        let idx = app.branches.iter().position(|b| b.name == "main").unwrap();
        app.branch_selected = idx;
        press(&mut app, KeyCode::Char('m'));
        match &app.view {
            View::MergePick {
                source_branch,
                targets,
                ..
            } => {
                assert_eq!(source_branch, "main");
                assert!(targets.iter().any(|t| t.name == "feature"));
            }
            _ => panic!("expected the merge picker"),
        }
    }

    #[test]
    fn open_key_picks_then_runs_a_single_open_command_with_expansion() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        let path = app.worktrees[app.selected].path.clone();
        app.ctx.config.open_command = vec!["touch {path}/opened.txt".into()];
        press(&mut app, KeyCode::Char('o'));
        // A lone command still opens the picker so it is confirmed first.
        match &app.view {
            View::OpenCommand { commands, .. } => assert_eq!(commands.len(), 1),
            _ => panic!("expected open-command picker"),
        }
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::List));
        assert!(
            app.message.as_deref().unwrap().contains("ran"),
            "{:?}",
            app.message
        );
        // Give the detached shell a moment to create the file.
        let marker = Path::new(&path).join("opened.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "expanded open_command should have run");
    }

    /// A terminal-mode command doesn't spawn anything from inside the TUI: it
    /// records what to run and quits, so `tui::run` can hand this terminal over
    /// once ratatui has restored it.
    #[test]
    fn a_terminal_mode_command_quits_and_hands_over_the_terminal() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        let path = app.worktrees[app.selected].path.clone();
        app.ctx.config.open_command =
            vec![OpenCommand::new("nvim {path}").with_mode(CommandMode::Terminal)];
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Enter);
        assert!(app.quit, "the TUI stands down for a terminal command");
        assert_eq!(
            app.exec_on_exit,
            Some((format!("nvim {path}"), path.clone())),
            "the expanded command and its directory are handed off"
        );
    }

    /// A background command is the opposite: it spawns detached and the TUI
    /// stays up, which is what a GUI editor needs.
    #[test]
    fn a_background_mode_command_leaves_the_tui_running() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        app.ctx.config.open_command = vec![OpenCommand::new("true")];
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Enter);
        assert!(!app.quit);
        assert!(app.exec_on_exit.is_none());
    }

    /// `g` and `t` in the command editor flip the two per-command toggles, and
    /// `[ done ]` saves a global command to the global file while the repo's
    /// own `.wtm.toml` keeps only its own.
    #[test]
    fn command_editor_toggles_global_and_terminal_then_saves_to_the_right_file() {
        let (tmp, mut app) = test_app();
        goto_settings(&mut app);
        press(&mut app, KeyCode::Down); // open_command row
        press(&mut app, KeyCode::Enter); // open the list editor
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "cursor {path}");
        press(&mut app, KeyCode::Enter); // commit the entry, cursor lands on it
        press(&mut app, KeyCode::Char('g')); // save it globally
        press(&mut app, KeyCode::Char('t')); // and run it in the terminal
        let list = app.settings.open_list.as_ref().unwrap();
        assert!(list.commands[0].global);
        assert_eq!(list.commands[0].mode, CommandMode::Terminal);

        for _ in list.selected..list.done_row() {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter); // [ done ] writes both files

        let repo_text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(
            !repo_text.contains("open_command"),
            "a global command must not land in the repo file: {repo_text}"
        );
        let global_text = std::fs::read_to_string(tmp.path().join("global.toml")).unwrap();
        assert!(global_text.contains("cursor {path}"), "{global_text}");
        assert!(
            global_text.contains(r#"mode = "terminal""#),
            "{global_text}"
        );
    }

    #[test]
    fn open_key_picks_among_multiple_open_commands() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        app.ctx.config.open_command =
            vec!["touch {path}/a.txt".into(), "touch {path}/b.txt".into()];
        press(&mut app, KeyCode::Char('o'));
        match &app.view {
            View::OpenCommand {
                commands, selected, ..
            } => {
                assert_eq!(commands.len(), 2);
                assert_eq!(*selected, 0);
            }
            _ => panic!("expected open-command picker"),
        }
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::List));
        let path = PathBuf::from(
            app.worktrees
                .iter()
                .find(|w| w.name == "feature")
                .unwrap()
                .path
                .clone(),
        );
        let marker = path.join("b.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "second open_command should have run");
        assert!(
            !path.join("a.txt").exists(),
            "first command was not selected"
        );
    }

    /// The picker expands against whichever worktree is highlighted, so the
    /// preview and the command that runs both point at that worktree.
    #[test]
    fn open_command_picker_expands_for_the_selected_worktree() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "alpha");
        add_and_select_worktree(&mut app, "beta");
        app.ctx.config.open_command = vec!["cursor {path} # {name}".into()];
        let selected = app.worktrees[app.selected].clone();
        press(&mut app, KeyCode::Char('o'));
        let View::OpenCommand {
            name,
            path,
            branch,
            status,
            commands,
            ..
        } = &app.view
        else {
            panic!("expected open-command picker");
        };
        assert_eq!(name, &selected.name);
        assert_eq!(path, &selected.path);
        let preview = crate::config::expand_open_command(
            &commands[0].command,
            &crate::config::OpenCommandVars {
                path,
                name,
                branch,
                status,
            },
        );
        assert_eq!(
            preview,
            format!("cursor {} # {}", selected.path, selected.name)
        );
    }

    #[test]
    fn open_key_prompts_when_no_open_command_is_set() {
        let (_tmp, mut app) = test_app();
        assert!(app.ctx.config.open_command.is_empty());
        press(&mut app, KeyCode::Char('o'));
        assert!(matches!(app.view, View::RunCommand { .. }));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
    }

    #[test]
    fn header_shows_the_selected_worktree_path() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        let path = app.worktrees[app.selected].path.clone();
        let repo = app.ctx.repo_root.display().to_string();
        assert_ne!(
            path, repo,
            "fixture worktree should sit outside the main root"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let header: String = (0..120).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(
            header.contains(&path),
            "header should show selected worktree path {path}: {header}"
        );
        // The path cell is the worktree folder, not a bare repo-root display
        // (sibling worktrees nest under `{repo}-worktrees/…`, so a naive
        // "does not contain repo root" check would false-positive).
        assert!(
            header.contains("proj-worktrees/feature") || header.ends_with(&format!("{path} ")),
            "header path cell should be the worktree: {header}"
        );
    }

    #[test]
    fn move_changes_key_opens_picker_and_moves_changes() {
        let (_tmp, mut app) = test_app();
        // The main worktree carries an untracked `.wtm.toml` in this fixture,
        // so use two fresh worktrees as clean source/destination instead.
        add_and_select_worktree(&mut app, "feature");
        add_and_select_worktree(&mut app, "other");
        let feat_path = PathBuf::from(
            app.worktrees
                .iter()
                .find(|w| w.name == "feature")
                .unwrap()
                .path
                .clone(),
        );
        std::fs::write(feat_path.join("a.txt"), "a\n").unwrap();
        app.refresh();
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == "feature")
            .unwrap();

        press(&mut app, KeyCode::Char('m'));
        match &app.view {
            View::MoveChanges { from, targets, .. } => {
                assert_eq!(from, "feature");
                assert!(targets.iter().any(|t| t.name == "other"));
                assert!(
                    !targets.iter().any(|t| t.name == "feature"),
                    "can't move into itself"
                );
            }
            _ => panic!("expected the move-changes picker"),
        }
        if let View::MoveChanges {
            selected, targets, ..
        } = &mut app.view
        {
            *selected = targets.iter().position(|t| t.name == "other").unwrap();
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);

        assert!(
            app.message.as_deref().unwrap_or("").contains("moved"),
            "message: {:?}, error: {:?}",
            app.message,
            app.error
        );
        app.refresh();
        let feature = app.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert_eq!(feature.dirty, 0, "changes moved out of feature");
        let other = app.worktrees.iter().find(|w| w.name == "other").unwrap();
        assert!(other.dirty > 0, "changes landed in other");
        assert_eq!(
            std::fs::read_to_string(PathBuf::from(&other.path).join("a.txt")).unwrap(),
            "a\n"
        );
    }

    #[test]
    fn move_changes_key_with_no_changes_shows_a_message() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.view, View::List));
        assert!(app.message.as_deref().unwrap().contains("no changes"));
    }

    #[test]
    fn merge_conflict_opens_resolver_and_completes() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);

        // The resolver opened on the conflicted file with one undecided hunk.
        match &app.view {
            View::ConflictResolver {
                target,
                files,
                current,
                ..
            } => {
                assert_eq!(target, "feature");
                assert_eq!(files, &vec!["shared.txt".to_string()]);
                let rf = current.as_ref().expect("file loaded with a hunk");
                assert_eq!(rf.actions.len(), 1);
                assert!(rf.actions[0].is_none());
            }
            _ => panic!("expected the conflict resolver"),
        }

        // Staging before choosing a side is refused (still unresolved).
        press(&mut app, KeyCode::Char('w'));
        assert!(matches!(app.view, View::ConflictResolver { .. }));

        // Pick a side, stage the file, then complete the merge.
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('c'));

        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
    }

    #[test]
    fn resolver_manual_edit_writes_hand_edited_result() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);

        // `e` opens the manual editor seeded with both sides; inserting a
        // character and Ctrl+S resolves the hunk and writes the file.
        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(app.modal, Some(Modal::HunkEditor(_))));
        press(&mut app, KeyCode::Char('Z'));
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.modal.is_none(), "editor closes on save");

        // Ctrl+S must reach the disk, not just app state: an edit held only in
        // memory is invisible to git and to the user's own editor. Ours is the
        // feature side; the seed was ours-then-theirs with a 'Z' at the front.
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            "Zfeature version\nmain version\n",
            "the edit is on disk before anything is staged"
        );
        // That was the only hunk, so re-reading the file finds nothing left to
        // resolve.
        match &app.view {
            View::ConflictResolver { current, .. } => {
                assert!(current.is_none(), "no conflict hunks remain");
            }
            _ => panic!("expected the conflict resolver"),
        }

        // Saving does not stage: git still sees the path as unmerged until the
        // user says so with `w`.
        assert_eq!(
            crate::git::conflicted_files(&feat).unwrap(),
            vec!["shared.txt".to_string()]
        );

        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            "Zfeature version\nmain version\n"
        );
    }

    /// The old editor kept manual edits in memory only, so moving to another
    /// file silently threw them away. Saving writes through to disk, so the
    /// edit has to survive the round trip.
    #[test]
    fn resolver_manual_edit_survives_moving_between_files() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver_two_files(&mut app);

        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Char('Z'));
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        let first = match &app.view {
            View::ConflictResolver { files, file, .. } => files[*file].clone(),
            _ => panic!("expected the conflict resolver"),
        };
        let saved = std::fs::read_to_string(feat.join(&first)).unwrap();
        assert!(saved.starts_with('Z'), "edit landed on disk: {saved}");

        // Move to the other conflicted file and back.
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Left);
        assert_eq!(
            std::fs::read_to_string(feat.join(&first)).unwrap(),
            saved,
            "the edit is still there after navigating away and back"
        );
    }

    /// A file with several hunks can be saved with only some of them decided:
    /// the settled ones are written out resolved, the rest keep their markers.
    #[test]
    fn resolver_saves_partial_resolutions_without_staging() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver_multi_hunk(&mut app);

        // Decide only the first of the two hunks, then save.
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('w'));

        let text = std::fs::read_to_string(feat.join("multi.txt")).unwrap();
        assert_eq!(
            text.matches("<<<<<<<").count(),
            1,
            "one hunk resolved, one still marked: {text}"
        );
        // Still unmerged, because it would be wrong to stage conflict markers.
        assert_eq!(
            crate::git::conflicted_files(&feat).unwrap(),
            vec!["multi.txt".to_string()],
            "a partly-resolved file is saved but not staged"
        );
        assert!(matches!(app.view, View::ConflictResolver { .. }));

        // Decide the one that is left; now `w` stages it.
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('w'));
        assert!(
            crate::git::conflicted_files(&feat).unwrap().is_empty(),
            "fully resolved, so staged"
        );
    }

    /// `a` is the escape hatch for a conflict fixed in the user's own editor:
    /// stage exactly what is on disk. It must refuse while markers remain.
    #[test]
    fn resolver_stages_an_externally_resolved_file() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);

        // Markers still present: refuse rather than commit them.
        press(&mut app, KeyCode::Char('a'));
        assert!(app.error.is_some(), "refuses while markers remain");
        assert!(
            app.error.as_ref().unwrap().contains("unresolved conflict"),
            "{:?}",
            app.error
        );
        press(&mut app, KeyCode::Esc); // dismiss the error popup

        // Resolve it "in another editor", then mark it resolved.
        std::fs::write(feat.join("shared.txt"), "hand written\n").unwrap();
        press(&mut app, KeyCode::Char('a'));
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(
            crate::git::conflicted_files(&feat).unwrap().is_empty(),
            "staged what was on disk"
        );

        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            "hand written\n"
        );
    }

    /// `r` re-reads the conflict list, so a file resolved and staged outside
    /// wtm drops off without needing a restart.
    #[test]
    fn resolver_reload_picks_up_external_resolutions() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);

        std::fs::write(feat.join("shared.txt"), "fixed elsewhere\n").unwrap();
        crate::git::stage_paths(&feat, &["shared.txt".to_string()]).unwrap();

        press(&mut app, KeyCode::Char('r'));
        match &app.view {
            View::ConflictResolver { files, .. } => {
                assert!(files.is_empty(), "resolved file dropped off: {files:?}");
            }
            _ => panic!("expected the conflict resolver"),
        }
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
    }

    /// `⇧E` hands the user the whole file, markers and all, for the fixes the
    /// per-hunk choices can't express. Ctrl+S has to land on disk and re-read
    /// the file, without staging it.
    #[test]
    fn resolver_file_editor_writes_the_whole_file_to_disk() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);

        press(&mut app, KeyCode::Char('E'));
        match &app.modal {
            Some(Modal::FileEditor { path, editor }) => {
                assert_eq!(path, "shared.txt");
                assert!(
                    editor.text().contains("<<<<<<<"),
                    "the editor gets the raw file, markers included: {}",
                    editor.text()
                );
            }
            _ => panic!("expected the whole-file editor"),
        }

        // Stand in for the user typing a hand-merged result.
        if let Some(Modal::FileEditor { editor, .. }) = &mut app.modal {
            *editor = TextArea::new("feature version\nmain version\n");
        }
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert!(app.modal.is_none(), "editor closes on save");
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            "feature version\nmain version\n",
            "the edit is on disk"
        );
        match &app.view {
            View::ConflictResolver { current, .. } => {
                assert!(current.is_none(), "no conflict hunks remain");
            }
            _ => panic!("expected the conflict resolver"),
        }
        // Saving is not staging: git still sees the path as unmerged until `w`.
        assert_eq!(
            crate::git::conflicted_files(&feat).unwrap(),
            vec!["shared.txt".to_string()]
        );

        press(&mut app, KeyCode::Char('w'));
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
    }

    /// Opening the whole-file editor must not hand back a stale copy: choices
    /// already made in the resolver are flushed to disk first, or saving the
    /// edit would silently undo them.
    #[test]
    fn resolver_file_editor_starts_from_choices_already_made() {
        let (_tmp, mut app) = test_app();
        into_conflict_resolver(&mut app);

        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('E'));
        match &app.modal {
            Some(Modal::FileEditor { editor, .. }) => assert_eq!(
                editor.text(),
                "feature version\n",
                "the kept side is already applied in the text being edited"
            ),
            _ => panic!("expected the whole-file editor"),
        }
    }

    #[test]
    fn resolver_file_editor_esc_discards() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);
        let before = std::fs::read_to_string(feat.join("shared.txt")).unwrap();

        press(&mut app, KeyCode::Char('E'));
        press(&mut app, KeyCode::Char('Z'));
        press(&mut app, KeyCode::Esc);

        assert!(app.modal.is_none());
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            before,
            "a discarded edit never reaches the file"
        );
    }

    /// Ctrl-chords other than Ctrl+S are not text: they must not leave stray
    /// letters in the buffer.
    #[test]
    fn hunk_editor_ignores_other_control_chords() {
        let (_tmp, mut app) = test_app();
        into_conflict_resolver(&mut app);
        press(&mut app, KeyCode::Char('e'));
        let before = match &app.modal {
            Some(Modal::HunkEditor(ed)) => ed.text(),
            _ => panic!("expected the hunk editor"),
        };
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        match &app.modal {
            Some(Modal::HunkEditor(ed)) => assert_eq!(ed.text(), before),
            _ => panic!("editor should still be open"),
        }
    }

    #[test]
    fn resolver_manual_edit_esc_discards() {
        let (_tmp, mut app) = test_app();
        into_conflict_resolver(&mut app);
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Char('Z'));
        press(&mut app, KeyCode::Esc);
        // Esc drops the editor without recording an action.
        assert!(app.modal.is_none());
        match &app.view {
            View::ConflictResolver {
                current: Some(rf), ..
            } => {
                assert!(
                    rf.actions[0].is_none(),
                    "discarded edit leaves hunk undecided"
                );
            }
            _ => panic!("expected the conflict resolver"),
        }
    }

    #[test]
    fn hunk_editor_edits_and_round_trips() {
        // Seed with two lines; the trailing newline must survive.
        let mut ed = TextArea::new("ab\ncd\n");
        assert_eq!(ed.lines, vec!["ab", "cd"]);
        // Insert at the front of line 0.
        ed.on_key(KeyEvent::from(KeyCode::Char('X')));
        assert_eq!(ed.lines[0], "Xab");
        // Enter splits after the cursor (now past 'X'), so line 0 becomes "X".
        ed.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(ed.lines, vec!["X", "ab", "cd"]);
        // Backspace at column 0 joins this line onto the previous one.
        ed.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(ed.lines, vec!["Xab", "cd"]);
        assert_eq!(ed.text(), "Xab\ncd\n");
    }

    #[test]
    fn create_dialog_lists_remote_only_branches() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // Simulate a teammate's branch that was fetched into a remote-tracking
        // ref but has no local branch of the same name.
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        git(
            &root,
            &["update-ref", "refs/remotes/origin/teammate", sha.trim()],
        );
        press(&mut app, KeyCode::Char('n'));
        // Filter to the teammate branch and select it in the checkout list.
        type_str(&mut app, "teammate");
        match &app.view {
            View::Create { branches, .. } => {
                let c = branches
                    .iter()
                    .find(|c| c.branch == "teammate")
                    .expect("remote-only branch is offered for checkout");
                assert_eq!(c.remote.as_deref(), Some("origin/teammate"));
            }
            _ => panic!("expected create dialog"),
        }
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        wait_creating(&mut app, |_, done| done);
        press(&mut app, KeyCode::Enter);
        // Checking out a remote-only branch creates a local branch and worktree.
        assert!(crate::git::branch_exists(&root, "teammate"));
        assert!(app.worktrees.iter().any(|w| w.name == "teammate"));
    }

    #[test]
    fn create_dialog_filters_checkout_list_by_typed_text() {
        let (_tmp, mut app) = test_app();
        for b in ["alpha", "beta", "alpine"] {
            git(&app.ctx.repo_root, &["branch", b]);
        }
        press(&mut app, KeyCode::Char('n'));
        // Typing "alp" narrows the checkout list to the two matching branches;
        // the new-branch row (0) still offers to create "alp".
        type_str(&mut app, "alp");
        let filtered = match &app.view {
            View::Create { branches, name, .. } => filtered_candidates(branches, name.as_str()),
            _ => panic!("expected create dialog"),
        };
        let names: Vec<String> = match &app.view {
            View::Create { branches, .. } => filtered
                .iter()
                .map(|&i| branches[i].branch.clone())
                .collect(),
            _ => unreachable!(),
        };
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"alpine".to_string()));
        assert!(!names.contains(&"beta".to_string()));

        // ↓ enters the filtered list and Enter checks out a matching branch.
        press(&mut app, KeyCode::Down);
        let expected = match &app.view {
            View::Create {
                branches,
                name,
                selected,
                ..
            } => {
                let f = filtered_candidates(branches, name.as_str());
                branches[f[*selected - 1]].branch.clone()
            }
            _ => panic!("expected create dialog"),
        };
        assert!(expected == "alpha" || expected == "alpine");
    }

    #[test]
    fn resolver_abort_recovers_the_worktree() {
        let (_tmp, mut app) = test_app();
        let feat = into_conflict_resolver(&mut app);
        assert!(crate::git::is_merging(&feat));

        // `x` arms the confirmation; Esc backs out without aborting.
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm {
                action: ModalAction::ResolverAbort,
                ..
            })
        ));
        assert!(matches!(app.view, View::ConflictResolver { .. }));
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
        assert!(matches!(app.view, View::ConflictResolver { .. }));
        assert!(crate::git::is_merging(&feat));

        // Confirming the abort restores the pre-merge state.
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::is_merging(&feat));
        assert_eq!(
            std::fs::read_to_string(feat.join("shared.txt")).unwrap(),
            "feature version\n"
        );
    }

    #[test]
    fn resolver_hunk_action_selection_updates_state() {
        let (_tmp, mut app) = test_app();
        into_conflict_resolver(&mut app);
        // `t` records "keep theirs" for the current hunk.
        press(&mut app, KeyCode::Char('t'));
        match &app.view {
            View::ConflictResolver {
                current: Some(rf), ..
            } => {
                assert_eq!(rf.actions[0], Some(ResolutionAction::KeepTheirs));
            }
            _ => panic!("expected the resolver with a loaded file"),
        }
        // `b` overrides it with "keep both".
        press(&mut app, KeyCode::Char('b'));
        match &app.view {
            View::ConflictResolver {
                current: Some(rf), ..
            } => {
                assert_eq!(rf.actions[0], Some(ResolutionAction::KeepBoth));
            }
            _ => panic!("expected the resolver with a loaded file"),
        }
    }

    #[test]
    fn update_key_merges_default_branch_into_worktree() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        // A new commit on main that the feature worktree doesn't have yet.
        std::fs::write(app.ctx.repo_root.join("newfile.txt"), "x\n").unwrap();
        git(&app.ctx.repo_root, &["add", "."]);
        git(&app.ctx.repo_root, &["commit", "-m", "new on main"]);
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == "feature")
            .unwrap();
        press(&mut app, KeyCode::Char('u'));
        settle(&mut app);
        // A clean update lands back on the list with main's file pulled in.
        assert!(matches!(app.view, View::List));
        let feat = app.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert!(
            std::path::Path::new(&feat.path)
                .join("newfile.txt")
                .exists()
        );
    }

    /// Item 6: updating a worktree that has uncommitted changes prompts before
    /// merging; choosing "stash, update, reapply" keeps the local edit.
    #[test]
    fn update_on_dirty_worktree_offers_to_stash_and_reapplies() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        let feat_path = app
            .worktrees
            .iter()
            .find(|w| w.name == "feature")
            .unwrap()
            .path
            .clone();
        // Advance main so an update has something to merge.
        std::fs::write(app.ctx.repo_root.join("newfile.txt"), "x\n").unwrap();
        git(&app.ctx.repo_root, &["add", "."]);
        git(&app.ctx.repo_root, &["commit", "-m", "new on main"]);
        // Leave an uncommitted change in the worktree.
        std::fs::write(std::path::Path::new(&feat_path).join("wip.txt"), "wip\n").unwrap();
        app.refresh();
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == "feature")
            .unwrap();

        // `u` now asks how to handle the dirty tree instead of updating blindly.
        press(&mut app, KeyCode::Char('u'));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm {
                action: ModalAction::UpdateStash { .. },
                ..
            })
        ));
        // Default choice (0) is stash+update+reapply.
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(matches!(app.view, View::List));
        let dir = std::path::Path::new(&feat_path);
        assert!(dir.join("newfile.txt").exists(), "mainline change merged");
        assert_eq!(
            std::fs::read_to_string(dir.join("wip.txt")).unwrap(),
            "wip\n",
            "local edit reapplied after update"
        );
    }

    #[test]
    fn flat_rows_drop_folder_grouping() {
        let files = vec![
            StatusEntry {
                code: " M".to_string(),
                path: "src/app.rs".to_string(),
            },
            StatusEntry {
                code: " M".to_string(),
                path: "README.md".to_string(),
            },
        ];
        // The tree groups the src/ file under a folder row; the flat list has
        // only file rows, each labelled by its full path.
        let tree = build_rows(&files, true, &HashSet::new());
        assert!(tree.iter().any(|r| matches!(r, DiffRow::Folder { .. })));
        let flat = build_rows(&files, false, &HashSet::new());
        assert!(flat.iter().all(|r| matches!(r, DiffRow::File { .. })));
        let labels: Vec<&str> = flat
            .iter()
            .filter_map(|r| match r {
                DiffRow::File { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["README.md", "src/app.rs"]);
    }

    #[test]
    fn collapsed_folders_hide_their_files_in_the_rows() {
        let files = vec![
            StatusEntry {
                code: " M".into(),
                path: "src/tui/app.rs".into(),
            },
            StatusEntry {
                code: " M".into(),
                path: "README.md".into(),
            },
        ];
        let collapsed = HashSet::from(["src/".to_string()]);
        let rows = build_diff_rows(&files, &collapsed);
        // README.md, then a single collapsed src/ row; src/tui/ and the file
        // beneath it are hidden.
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows.get(1),
            Some(DiffRow::Folder {
                collapsed: true,
                ..
            })
        ));
    }

    #[test]
    fn folders_collapse_and_expand_in_the_diff_tree() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "b\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);
        select_diff_folder(&mut app, "src/");

        // ← collapses the folder under the cursor: its files leave the rows.
        press(&mut app, KeyCode::Left);
        assert!(app.collapsed_folders.contains("src/"));
        {
            let c = &app.changes;
            assert!(matches!(
                c.rows.get(c.selected),
                Some(DiffRow::Folder {
                    collapsed: true,
                    ..
                })
            ));
            assert!(
                !c.rows
                    .iter()
                    .any(|r| matches!(r, DiffRow::File { label, .. } if label == "a.rs"))
            );
        }

        // → expands it again.
        press(&mut app, KeyCode::Right);
        assert!(!app.collapsed_folders.contains("src/"));
        assert!(
            app.changes
                .rows
                .iter()
                .any(|r| matches!(r, DiffRow::File { label, .. } if label == "a.rs"))
        );

        // Enter toggles, and a refresh (`r`) keeps the collapse.
        press(&mut app, KeyCode::Enter);
        assert!(app.collapsed_folders.contains("src/"));
        press(&mut app, KeyCode::Char('r'));
        assert!(
            !app.changes
                .rows
                .iter()
                .any(|r| matches!(r, DiffRow::File { label, .. } if label == "a.rs"))
        );
    }

    #[test]
    fn left_on_a_file_jumps_to_its_parent_folder() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "a\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);
        select_diff_file(&mut app, "src/a.rs");
        press(&mut app, KeyCode::Left);
        let c = &app.changes;
        assert!(matches!(
            c.rows.get(c.selected),
            Some(DiffRow::Folder { prefix, .. }) if prefix == "src/"
        ));
    }

    #[test]
    fn wheel_moves_the_file_cursor_over_the_list_and_scrolls_the_diff_elsewhere() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Enter);
        settle_diff(&mut app);
        // Pretend the renderer recorded the file list panel on the left.
        let len = app.changes.rows.len();
        app.row_list = Some(RowList {
            inner: Rect::new(0, 1, 36, 20),
            header: 0,
            offset: 0,
            len,
        });
        let wheel = |app: &mut App, kind, column| {
            app.on_mouse(MouseEvent {
                kind,
                column,
                row: 5,
                modifiers: KeyModifiers::empty(),
            });
        };
        // Over the list, a notch moves the file cursor and leaves the diff
        // scroll alone.
        wheel(&mut app, MouseEventKind::ScrollDown, 5);
        assert_eq!(app.changes.selected, 1);
        assert_eq!(app.changes.scroll, 0);
        // Right of the list (the diff panel), the wheel scrolls the text.
        wheel(&mut app, MouseEventKind::ScrollDown, 60);
        assert_eq!(app.changes.selected, 1, "cursor stays put");
        assert_eq!(app.changes.scroll, 3);
        // And scrolling up over the list moves the cursor back.
        wheel(&mut app, MouseEventKind::ScrollUp, 5);
        assert_eq!(app.changes.selected, 0);
    }

    #[test]
    fn switch_picker_lists_available_branches_and_switches() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "spare"]);
        app.refresh();
        app.selected = 0;
        // `b` on the Worktrees tab opens the switch picker for the selected
        // worktree, listing branches not checked out anywhere.
        press(&mut app, KeyCode::Char('b'));
        match &app.view {
            View::Switch { branches, .. } => {
                assert!(branches.iter().any(|b| b.branch == "spare"));
                // The worktree's own current branch is not offered.
                assert!(
                    !branches
                        .iter()
                        .any(|b| b.branch == "main" || b.branch == "master")
                );
            }
            _ => panic!("expected the switch picker"),
        }
        // Select "spare" and switch onto it.
        if let View::Switch {
            branches, selected, ..
        } = &mut app.view
        {
            *selected = branches.iter().position(|b| b.branch == "spare").unwrap();
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("spare"))
        );
    }

    #[test]
    fn switch_filter_narrows_branch_list_and_enter_targets_match() {
        let (_tmp, mut app) = test_app();
        for name in ["feature-auth", "feature-billing", "hotfix-1"] {
            git(&app.ctx.repo_root, &["branch", name]);
        }
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('b'));
        match &app.view {
            View::Switch { branches, .. } => assert_eq!(branches.len(), 3),
            _ => panic!("expected the switch picker"),
        }
        // Typing narrows the filtered set (case-insensitive substring match).
        type_str(&mut app, "FEATURE");
        assert_eq!(
            switch_matches(&app),
            vec!["feature-auth", "feature-billing"]
        );
        // Narrowing further to a single match, Enter switches to that match
        // (not to an index into the full, unfiltered branch list).
        type_str(&mut app, "-billing");
        assert_eq!(switch_matches(&app), vec!["feature-billing"]);
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("feature-billing"))
        );
    }

    #[test]
    fn switch_picker_lists_remote_only_branches_and_checks_them_out() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        make_remote_ref(&root, "origin", "teammate");
        app.refresh();
        app.selected = 0;

        press(&mut app, KeyCode::Char('b'));
        match &app.view {
            View::Switch { branches, .. } => {
                let c = branches
                    .iter()
                    .find(|c| c.branch == "teammate")
                    .expect("remote-only branch is offered to switch onto");
                assert_eq!(c.remote.as_deref(), Some("origin/teammate"));
            }
            _ => panic!("expected the switch picker"),
        }
        // Switching onto it creates the local branch that tracks the remote.
        type_str(&mut app, "teammate");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(crate::git::branch_exists(&root, "teammate"));
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("teammate"))
        );
    }

    #[test]
    fn switch_enter_with_no_match_tries_the_typed_branch() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('b'));

        // A branch created outside the app is absent from the picker's list, but
        // typing its name and hitting Enter still switches onto it.
        git(&root, &["branch", "late"]);
        type_str(&mut app, "late");
        assert!(switch_matches(&app).is_empty());
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("late"))
        );
    }

    #[test]
    fn switch_enter_with_unknown_typed_branch_creates_it() {
        let (_tmp, mut app) = test_app();
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('b'));
        // Typing a name that matches no existing branch and hitting Enter creates
        // a new branch of that name and switches the worktree onto it.
        type_str(&mut app, "brand-new");
        assert!(switch_matches(&app).is_empty());
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert_eq!(app.error, None, "creating a new branch should not error");
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("brand-new")),
            "the worktree switched onto the newly created branch"
        );
    }

    #[test]
    fn switch_esc_clears_filter_before_closing() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "spare"]);
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('b'));
        type_str(&mut app, "sp");
        press(&mut app, KeyCode::Esc);
        match &app.view {
            View::Switch { filter, .. } => {
                assert!(filter.as_str().is_empty(), "first Esc clears the filter");
            }
            _ => panic!("expected the switch picker to stay open"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(
            matches!(app.view, View::List),
            "second Esc closes the picker"
        );
    }

    #[test]
    fn switch_j_and_k_type_into_filter_instead_of_navigating() {
        // j/k are printable characters a branch name could contain, so unlike
        // most lists in this app they must feed the filter, not move the
        // cursor; only the arrow keys navigate here.
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "jkbranch"]);
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        match &app.view {
            View::Switch {
                filter, selected, ..
            } => {
                assert_eq!(filter.as_str(), "jk");
                assert_eq!(*selected, 0);
            }
            _ => panic!("expected the switch picker"),
        }
    }

    #[test]
    fn log_overlay_opens_with_a_commit_cursor() {
        let (_tmp, mut app) = test_app();
        app.selected = 0;
        press(&mut app, KeyCode::Char('l'));
        match &app.view {
            View::Log {
                lines, selected, ..
            } => {
                assert!(!lines.is_empty());
                // The cursor lands on a real commit, not an art-only row.
                assert!(lines[*selected].entry.is_some());
            }
            _ => panic!("expected log overlay"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
    }

    /// Item 4: the commit browser renders the changed file and its diff.
    #[test]
    fn commit_browser_renders_files_and_diff() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("greet.txt"), "howdy\n").unwrap();
        git(&root, &["add", "greet.txt"]);
        git(&root, &["commit", "-m", "add greet"]);
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Enter);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(
            app.view,
            View::CommitDiff {
                pending: Some(_),
                ..
            }
        ) {
            app.poll_file_browser_diff_load();
            assert!(std::time::Instant::now() < deadline, "diff load timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| super::super::ui::draw(frame, &mut app))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let screen: String = (0..24)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(screen.contains("greet.txt"), "file listed:\n{screen}");
        assert!(screen.contains("howdy"), "diff shown:\n{screen}");
    }

    /// Item 4: from the log, Enter opens a read-only browser of the commit's
    /// changed files, and the selected file's diff loads (off-thread).
    #[test]
    fn log_enter_browses_a_commits_files() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("hello.txt"), "hi\n").unwrap();
        git(&root, &["add", "hello.txt"]);
        git(&root, &["commit", "-m", "add hello"]);
        app.refresh();
        app.selected = 0;
        press(&mut app, KeyCode::Char('l'));
        // The newest commit is at the top, under the cursor.
        press(&mut app, KeyCode::Enter);
        // Settle the async diff load.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(
            app.view,
            View::CommitDiff {
                pending: Some(_),
                ..
            }
        ) {
            app.poll_file_browser_diff_load();
            assert!(std::time::Instant::now() < deadline, "diff load timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        match &app.view {
            View::CommitDiff { files, content, .. } => {
                assert!(files.iter().any(|f| f.path == "hello.txt"), "{files:?}");
                assert!(
                    content.contains("hi"),
                    "diff shows the added line: {content}"
                );
            }
            _ => panic!("expected the commit browser"),
        }
        // Esc returns to the log.
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::Log { .. }));
    }

    /// End-to-end: a real merge in a real repo must come out of git, through the
    /// app, and onto the screen as a drawn tree.
    #[test]
    fn real_merge_renders_as_a_commit_tree() {
        let (_tmp, mut app) = test_app();
        let repo = app.ctx.repo_root.clone();
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

        app.refresh();
        press(&mut app, KeyCode::Char('l'));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app))
            .unwrap();
        let screen: Vec<String> = {
            let buffer = terminal.backend().buffer().clone();
            (0..12)
                .map(|y| {
                    (0..90)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect()
        };
        let body = screen.join("\n");
        // The merge and both sides are listed...
        for subject in ["merge feature", "main work", "feature work"] {
            assert!(body.contains(subject), "missing {subject:?} in:\n{body}");
        }
        // ...the tips are decorated the way git decorates them...
        assert!(body.contains("HEAD -> main"), "missing refs in:\n{body}");
        // ...and the topology is actually drawn, with a second lane branching
        // off and merging back rather than one flat column.
        assert!(body.contains('●'), "no commit markers in:\n{body}");
        assert!(
            body.contains('╲') || body.contains('╱'),
            "merge drew no branch lanes in:\n{body}"
        );
    }

    /// `t` swaps the log between the commit graph and a flat list, and the
    /// choice sticks for the next view opened.
    #[test]
    fn log_view_toggles_between_tree_and_flat() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.log_mode, LogMode::Tree);
        // Tree rows carry git's art; the flat list has none.
        match &app.view {
            View::Log { lines, .. } => assert!(lines.iter().any(|l| l.graph.contains('*'))),
            _ => panic!("expected the log overlay"),
        }
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.log_mode, LogMode::Flat);
        match &app.view {
            View::Log { lines, .. } => {
                assert!(!lines.is_empty());
                assert!(
                    lines
                        .iter()
                        .all(|l| l.graph.is_empty() && l.entry.is_some())
                );
            }
            _ => panic!("expected the log overlay"),
        }
        // The mode is remembered on the app, so the branch view opens flat too.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::BranchCommits { lines, .. } => {
                assert!(lines.iter().all(|l| l.graph.is_empty()))
            }
            _ => panic!("expected the branch commits view"),
        }
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.log_mode, LogMode::Tree);
    }

    /// In tree mode the cursor must step commit-to-commit, never landing on one
    /// of git's art-only connector rows (which carry nothing to cherry-pick).
    #[test]
    fn branch_commits_cursor_skips_graph_art_rows() {
        let lines = vec![
            GraphLine {
                graph: "* ".into(),
                entry: Some(log_entry("aaa")),
            },
            GraphLine {
                graph: "|\\".into(),
                entry: None,
            },
            GraphLine {
                graph: "| *".into(),
                entry: Some(log_entry("bbb")),
            },
        ];
        assert_eq!(first_commit_row(&lines), 0);
        // Moving down from the commit at 0 jumps the connector at 1.
        assert_eq!(seek_commit_row(&lines, 0, true), Some(2));
        assert_eq!(seek_commit_row(&lines, 2, false), Some(0));
        // At either end the cursor stays put rather than wrapping.
        assert_eq!(seek_commit_row(&lines, 2, true), None);
        assert_eq!(seek_commit_row(&lines, 0, false), None);
        // Leading art still resolves to the first real commit.
        let leading = vec![
            GraphLine {
                graph: "|\\".into(),
                entry: None,
            },
            GraphLine {
                graph: "* ".into(),
                entry: Some(log_entry("aaa")),
            },
        ];
        assert_eq!(first_commit_row(&leading), 1);
    }

    /// Space and `a` must not mark art rows, and a cherry-pick built from them
    /// must only carry real commits.
    #[test]
    fn branch_commits_marks_only_real_commits() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);
        press(&mut app, KeyCode::Enter);
        // Replace the loaded history with one containing a connector row.
        let View::BranchCommits { branch, .. } = &app.view else {
            panic!("expected the branch commits view");
        };
        let lines = vec![
            GraphLine {
                graph: "* ".into(),
                entry: Some(log_entry("aaa")),
            },
            GraphLine {
                graph: "|\\".into(),
                entry: None,
            },
        ];
        app.view = View::BranchCommits {
            branch: branch.clone(),
            marked: vec![false; lines.len()],
            lines,
            selected: 0,
        };
        // `a` marks every commit but leaves the art row alone.
        press(&mut app, KeyCode::Char('a'));
        match &app.view {
            View::BranchCommits { marked, .. } => assert_eq!(marked, &[true, false]),
            _ => panic!("expected the branch commits view"),
        }
        press(&mut app, KeyCode::Char('a'));
        match &app.view {
            View::BranchCommits { marked, .. } => assert_eq!(marked, &[false, false]),
            _ => panic!("expected the branch commits view"),
        }
    }

    /// Fetch and pull are wired up on the Branches tab. Without a remote the
    /// pull fails, which is what confirms it reached git rather than no-opping.
    #[test]
    fn branches_tab_pull_without_upstream_reports_error() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        settle_branches(&mut app);
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.view, View::Busy { .. }));
        settle_busy(&mut app);
        let err = app.error.clone().expect("expected an upstream error");
        assert!(err.contains("no upstream"), "unexpected error: {err}");
    }

    #[test]
    fn branches_tab_fetch_reloads_the_branch_list() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.view, View::Busy { .. }));
        settle_busy(&mut app);
        settle_branches(&mut app);
        // A repo with no remotes fetches nothing, and lands back on the tab.
        assert!(app.error.is_none(), "unexpected error: {:?}", app.error);
        assert_eq!(app.tab, Tab::Branches);
        assert!(matches!(app.view, View::List));
        assert!(!app.branches.is_empty());
    }

    /// The list reloads itself on a timer, keeping the cursor on the branch it
    /// was on even if the reload reorders things.
    #[test]
    fn auto_refresh_fires_on_the_interval_and_keeps_the_cursor() {
        let (_tmp, mut app) = test_app();
        app.worktrees.clear();
        app.tick();
        // Nothing reloads until the interval is up.
        assert!(app.worktrees.is_empty());

        app.last_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.tick();
        assert!(!app.worktrees.is_empty(), "expected the list to reload");

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        settle_branches(&mut app);
        let selected = app.branches[app.branch_selected].name.clone();
        app.last_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.tick();
        assert_eq!(app.branches[app.branch_selected].name, selected);
    }

    /// Auto-refresh must stay out of the way: it only runs on the plain list, so
    /// it can never reload state an overlay or a prompt is reading.
    #[test]
    fn auto_refresh_holds_off_during_overlays_and_prompts() {
        let (_tmp, mut app) = test_app();
        // An open overlay defers the refresh entirely.
        press(&mut app, KeyCode::Char('l'));
        app.last_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.worktrees.clear();
        app.tick();
        assert!(app.worktrees.is_empty(), "refreshed under an overlay");

        // So does typing a branch name on the Branches tab.
        app.view = View::List;
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(
            app.modal,
            Some(Modal::Prompt {
                action: ModalAction::BranchCreate,
                ..
            })
        ));
        app.last_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.branches.clear();
        app.tick();
        assert!(app.branches.is_empty(), "refreshed under a prompt");
    }

    #[test]
    fn pull_without_upstream_reports_error_via_busy() {
        let (_tmp, mut app) = test_app();
        app.selected = 0;
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.view, View::Busy { .. }));
        // Drain the background result like the event loop does.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.tick();
            if matches!(app.view, View::List) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "busy op timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Busy failures pop up the modal error box, not the header message.
        assert!(app.error.as_deref().unwrap().contains("no upstream"));
    }

    #[test]
    fn fetch_completes_via_busy() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.view, View::Busy { .. }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.tick();
            if matches!(app.view, View::List) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "fetch timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // No remotes configured, so the op reports that plainly.
        assert!(app.message.as_deref().unwrap().contains("no remotes"));
    }

    #[test]
    fn delete_keeps_branch_unless_toggled() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "keepme");
        press(&mut app, KeyCode::Char('d'));
        match &app.modal {
            Some(Modal::Confirm {
                selected,
                action: ModalAction::DeleteWorktree { branch, .. },
                ..
            }) => {
                assert_eq!(*selected, 0, "folder-only must be the default");
                assert_eq!(branch.as_deref(), Some("keepme"));
            }
            _ => panic!("expected delete dialog"),
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(matches!(app.view, View::List));
        assert!(!app.worktrees.iter().any(|w| w.name == "keepme"));
        assert!(
            crate::git::branch_exists(&app.ctx.repo_root, "keepme"),
            "branch must survive a folder-only delete"
        );
    }

    #[test]
    fn delete_can_also_remove_the_branch() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "dropme");
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Down); // toggle to "folder and branch"
        match &app.modal {
            Some(Modal::Confirm { selected, .. }) => assert_eq!(*selected, 1),
            _ => panic!("expected delete dialog"),
        }
        press(&mut app, KeyCode::Char('y'));
        settle(&mut app);
        assert!(!app.worktrees.iter().any(|w| w.name == "dropme"));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "dropme"));
    }

    #[test]
    fn delete_runs_through_the_busy_overlay() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "later");
        press(&mut app, KeyCode::Char('d'));
        // Confirming hands the removal to a background thread, so the overlay
        // shows immediately rather than freezing the UI.
        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(app.view, View::Busy { .. }),
            "delete should be backgrounded"
        );
        settle(&mut app);
        assert!(matches!(app.view, View::List));
        assert!(!app.worktrees.iter().any(|w| w.name == "later"));
    }

    /// After a folder-only delete, Changes/three-panel must not keep the removed
    /// name and then surface `no worktree named…` on the next diff refresh.
    #[test]
    fn delete_realigns_changes_so_refresh_diff_does_not_not_found() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "ghost");
        assert!(app.load_changes("ghost".into()));
        assert_eq!(app.changes.name, "ghost");

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Enter);
        settle(&mut app);

        assert!(
            app.error.is_none(),
            "delete itself must not error: {:?}",
            app.error
        );
        assert_ne!(
            app.changes.name, "ghost",
            "stale Changes target must be cleared"
        );
        assert!(!app.worktrees.iter().any(|w| w.name == "ghost"));

        // Force the periodic three-panel / Changes refresh path.
        app.three_panel = true;
        app.tab = Tab::Worktrees;
        app.changes.last_refresh = Instant::now() - DIFF_REFRESH_INTERVAL;
        app.tick();
        assert!(
            app.error.is_none(),
            "refresh after delete must not not_found: {:?}",
            app.error
        );
        assert!(
            app.changes.name.is_empty() || app.worktrees.iter().any(|w| w.name == app.changes.name),
            "Changes must point at a living worktree or be empty"
        );
    }

    /// Folder+branch delete has the same stale-Changes hazard via DeleteBranch.
    #[test]
    fn delete_with_branch_realigns_changes_without_not_found() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "vanish");
        assert!(app.load_changes("vanish".into()));

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Down); // folder + branch
        press(&mut app, KeyCode::Char('y'));
        settle(&mut app);

        assert!(app.error.is_none(), "unexpected error: {:?}", app.error);
        assert_ne!(app.changes.name, "vanish");
        app.three_panel = true;
        app.changes.last_refresh = Instant::now() - DIFF_REFRESH_INTERVAL;
        app.tick();
        assert!(
            app.error.is_none(),
            "post-delete tick must stay clean: {:?}",
            app.error
        );
    }

    /// A Changes target already missing from the list fails soft, not as a popup.
    #[test]
    fn refresh_diff_silently_realigns_a_missing_worktree() {
        let (_tmp, mut app) = test_app();
        app.changes.name = "nope".into();
        app.refresh_diff();
        assert!(
            app.error.is_none(),
            "ghost target must not popup: {:?}",
            app.error
        );
        assert!(app.changes.name.is_empty() || app.changes.name != "nope");
    }

    #[test]
    fn deleting_a_dirty_worktree_prompts_then_discards() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "messy");
        // Leave an untracked file so the worktree reads as dirty.
        let path = app.worktrees[app.selected].path.clone();
        std::fs::write(Path::new(&path).join("scratch.txt"), "work\n").unwrap();
        app.refresh();
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == "messy")
            .unwrap();

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(
                app.modal,
                Some(Modal::Confirm {
                    action: ModalAction::DeleteWorktreeDirty { .. },
                    ..
                })
            ),
            "a dirty worktree should open the stash/discard prompt"
        );
        // Move to "discard" (index 1) and confirm.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(matches!(app.view, View::List));
        assert!(!app.worktrees.iter().any(|w| w.name == "messy"));
    }

    #[test]
    fn deleting_an_unmerged_branch_prompts_to_force() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        // Commit on the worktree so its branch is not merged into main.
        let path = app.worktrees[app.selected].path.clone();
        std::fs::write(Path::new(&path).join("f.txt"), "x\n").unwrap();
        git(Path::new(&path), &["add", "."]);
        git(Path::new(&path), &["commit", "-m", "unmerged work"]);

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Down); // toggle "also delete branch"
        press(&mut app, KeyCode::Char('y'));
        settle(&mut app);
        // Folder removed synchronously, branch delete refused -> force prompt.
        match &app.modal {
            Some(Modal::Confirm {
                action: ModalAction::ForceBranch { branch },
                ..
            }) => {
                assert_eq!(branch, "feature");
            }
            _ => panic!("expected the force-branch prompt after an unmerged branch delete"),
        }
        assert!(matches!(app.view, View::List));
        assert!(!app.worktrees.iter().any(|w| w.name == "feature"));
        assert!(crate::git::branch_exists(&app.ctx.repo_root, "feature"));
        // Force the delete. The force option is the Shift-variant so a bare `f`
        // (fetch elsewhere) can't trigger it.
        press(&mut app, KeyCode::Char('F'));
        assert!(app.modal.is_none());
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "feature"));
    }

    /// A pull refused because the branch diverged opens the rebase prompt
    /// instead of the error box, and confirming retries the pull with a
    /// rebase.
    #[test]
    fn diverged_pull_prompts_to_rebase_and_retries() {
        let (tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        // Wire the repo to a bare origin, then diverge: one commit reaches the
        // remote from an independent clone, a different one lands locally.
        let bare = tmp.path().join("origin.git");
        git(
            tmp.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&root, &["push", "-u", "origin", "main"]);
        let clone = tmp.path().join("clone");
        git(
            tmp.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.email", "t@e.st"]);
        git(&clone, &["config", "user.name", "t"]);
        git(&clone, &["commit", "--allow-empty", "-m", "remote-work"]);
        git(&clone, &["push", "origin", "main"]);
        git(&root, &["commit", "--allow-empty", "-m", "local-work"]);

        press(&mut app, KeyCode::Char('p'));
        settle(&mut app);
        match &app.modal {
            Some(Modal::Confirm {
                action: ModalAction::PullRebase { name },
                ..
            }) => assert_eq!(name, "main"),
            _ => panic!("expected the rebase prompt after a diverged pull"),
        }
        assert!(matches!(app.view, View::List));
        assert_eq!(app.error, None, "the raw git error should be suppressed");

        // Confirming retries with a rebase: local work ends up on top.
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(matches!(app.view, View::List));
        assert_eq!(app.error, None);
        assert_eq!(app.message.as_deref(), Some("pulled 'main' with rebase"));
        let subject = crate::git::run(&root, &["log", "-1", "--format=%s"]).unwrap();
        assert_eq!(subject, "local-work");
    }

    /// Esc on the rebase prompt backs out without touching the branch.
    #[test]
    fn diverged_pull_prompt_can_be_dismissed() {
        let (_tmp, mut app) = test_app();
        app.open_pull_rebase_modal("main".into());
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
        assert!(matches!(app.view, View::List));
    }

    #[test]
    fn config_editor_edits_and_saves_settings() {
        let (_tmp, mut app) = test_app();
        goto_settings(&mut app);
        assert_eq!(app.tab, Tab::Settings);

        // Edit worktree_dir (row 0): clear, type "inside". Enter writes immediately.
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "inside");
        press(&mut app, KeyCode::Enter);
        assert!(app.message.as_deref().unwrap().contains("saved"));
        assert_eq!(app.ctx.config.worktree_dir.as_deref(), Some("inside"));

        // Move past open_command (row 1) to setup.copy (row 2) and set it.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, ".env, config/.env.local");
        press(&mut app, KeyCode::Enter);

        assert_eq!(app.tab, Tab::Settings, "saving stays on the tab");
        let text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(text.contains("worktree_dir = \"inside\""), "{text}");
        assert!(text.contains(".env"), "{text}");
        assert!(text.contains("config/.env.local"), "{text}");
    }

    /// The open_command row is edited as a list; finishing with `[ done ]`
    /// writes a TOML array with each command kept whole, commas and all.
    #[test]
    fn config_editor_saves_the_open_command_list() {
        let (_tmp, mut app) = test_app();
        goto_settings(&mut app);
        press(&mut app, KeyCode::Down); // open_command row
        press(&mut app, KeyCode::Enter); // open the list editor
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "cursor {path}");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "sh -c 'cd {path}, npm start'");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down); // add row
        press(&mut app, KeyCode::Down); // [ done ]
        press(&mut app, KeyCode::Enter);
        assert!(app.settings.open_list.is_none());
        assert!(
            app.message.as_deref().unwrap().contains("saved"),
            "{:?}",
            app.message
        );
        assert_eq!(
            command_texts(&app.ctx.config.open_command),
            ["cursor {path}", "sh -c 'cd {path}, npm start'"]
        );
        // Leaving and re-entering the tab reloads from disk; the list must
        // still be there ( [ done ] already wrote it ).
        app.select_tab(Tab::Worktrees);
        goto_settings(&mut app);
        assert_eq!(
            command_texts(&app.settings.fields.open_command),
            ["cursor {path}", "sh -c 'cd {path}, npm start'"]
        );
    }

    /// Finishing the open_command list with `[ done ]` persists to disk, so
    /// switching away from Settings and back does not wipe the commands.
    #[test]
    fn open_command_list_done_survives_leaving_settings() {
        let (_tmp, mut app) = test_app();
        goto_settings(&mut app);
        press(&mut app, KeyCode::Down); // open_command row
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "cursor {path}");
        press(&mut app, KeyCode::Enter);
        let list = app.settings.open_list.as_ref().unwrap();
        for _ in list.selected..list.done_row() {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            command_texts(&app.settings.fields.open_command),
            ["cursor {path}"]
        );

        press(&mut app, KeyCode::Tab); // leave Settings
        assert_ne!(app.tab, Tab::Settings);
        goto_settings(&mut app);
        assert_eq!(
            command_texts(&app.settings.fields.open_command),
            ["cursor {path}"],
            "reload after a tab switch must still see the saved command"
        );
        assert_eq!(
            command_texts(&app.ctx.config.open_command),
            ["cursor {path}"]
        );
    }

    /// The whole multi-command path in one go: build the list in Settings
    /// (adding, editing, and removing entries), save it, then go back to the
    /// Worktrees tab and run the second command from the picker. Saving has to
    /// reload the live config for `o` to see the new commands at all.
    #[test]
    fn settings_list_then_open_key_runs_the_chosen_command() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        let path = PathBuf::from(app.worktrees[app.selected].path.clone());

        goto_settings(&mut app);
        press(&mut app, KeyCode::Down); // open_command row
        press(&mut app, KeyCode::Enter); // open the list editor

        // Add the first command.
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "touch {path}/a.txt");
        press(&mut app, KeyCode::Enter);

        // Add a second, then edit it into its final form.
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "touch {path}/wrong.txt");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter); // Enter on the row edits it
        for _ in 0.."wrong.txt".len() {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "b.txt");
        press(&mut app, KeyCode::Enter);

        // Add a third and remove it again.
        press(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "touch {path}/c.txt");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(
            command_texts(&app.settings.open_list.as_ref().unwrap().commands),
            ["touch {path}/a.txt", "touch {path}/b.txt"]
        );

        // `[ done ]` writes the list to disk and reloads the live config.
        let list = app.settings.open_list.as_ref().unwrap();
        for _ in list.selected..list.done_row() {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.settings.open_list.is_none());
        assert_eq!(
            command_texts(&app.ctx.config.open_command),
            ["touch {path}/a.txt", "touch {path}/b.txt"]
        );

        app.select_tab(Tab::Worktrees);
        press(&mut app, KeyCode::Char('o'));
        let View::OpenCommand {
            commands,
            name,
            path: picked,
            branch,
            status,
            ..
        } = &app.view
        else {
            panic!("expected open-command picker");
        };
        assert_eq!(commands.len(), 2);
        // The picker previews the command expanded for this worktree, which is
        // what `Enter` then runs.
        assert_eq!(
            crate::config::expand_open_command(
                &commands[1].command,
                &crate::config::OpenCommandVars {
                    path: picked,
                    name,
                    branch,
                    status,
                },
            ),
            format!("touch {}/b.txt", path.display())
        );

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::List));

        let marker = path.join("b.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "the selected command should have run");
        assert!(
            !path.join("a.txt").exists(),
            "only the selected command runs"
        );
        assert!(!path.join("c.txt").exists(), "the removed command is gone");
    }

    #[test]
    fn config_editor_clearing_a_field_unsets_it() {
        let (_tmp, mut app) = test_app();
        std::fs::write(
            app.ctx.repo_root.join(".wtm.toml"),
            "worktree_dir = \"home\"\n[setup]\ncopy = [\".env\"]\n",
        )
        .unwrap();

        goto_settings(&mut app);
        // Row 0 (worktree_dir) should load the existing "home".
        assert_eq!(app.tab, Tab::Settings);
        assert_eq!(app.settings.fields.worktree_dir, "home");
        // Clear worktree_dir back to empty.
        press(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            press(&mut app, KeyCode::Backspace);
        }
        press(&mut app, KeyCode::Enter);
        // Enter commits the empty value and writes immediately.
        assert_eq!(app.tab, Tab::Settings);
        let text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(!text.contains("worktree_dir"), "should be unset: {text}");
        assert!(text.contains(".env"), "copy should remain: {text}");
    }

    #[test]
    fn config_editor_cancel_leaves_file_untouched() {
        let (_tmp, mut app) = test_app();
        let before = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        goto_settings(&mut app);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "home");
        press(&mut app, KeyCode::Esc); // cancel the in-progress edit
        press(&mut app, KeyCode::Esc); // quit
        assert!(matches!(app.view, View::List));
        let after = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert_eq!(before, after, "cancel must not write the file");
    }

    /// A release newer than whatever this build is, for the update-prompt tests.
    fn newer_release() -> Release {
        Release {
            tag: "v99.0.0".to_string(),
            version: "99.0.0".to_string(),
            url: "https://example.test/releases/tag/v99.0.0".to_string(),
        }
    }

    #[test]
    fn a_found_update_prompts_once_and_postponing_dismisses_it() {
        let (_tmp, mut app) = test_app();
        app.update_available = Some(newer_release());

        app.tick();
        let Some(Modal::Confirm { title, body, .. }) = &app.modal else {
            panic!("an available update should open the prompt");
        };
        assert!(title.contains("99.0.0"), "{title}");
        let text = body
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(update::CURRENT_VERSION),
            "current version missing: {text}"
        );
        // No API means no notes body, so the prompt links to the release page.
        assert!(
            text.contains("https://example.test/releases/tag/v99.0.0"),
            "release page link missing: {text}"
        );

        // "not now" (option 1) closes it without installing anything.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_none());
        assert!(!matches!(app.view, View::Busy { .. }), "must not install");

        // Later ticks must not nag again, even though the release is still known
        // (the Settings tab keeps showing it).
        app.tick();
        app.tick();
        assert!(app.modal.is_none(), "postponing must survive later ticks");
        assert!(app.update_available.is_some());
    }

    #[test]
    fn the_update_prompt_waits_until_the_screen_is_free() {
        let (_tmp, mut app) = test_app();
        app.update_available = Some(newer_release());
        // Drill into a dialog: an update must never interrupt work in progress.
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.view, View::Create { .. }));

        app.tick();
        assert!(app.modal.is_none(), "no prompt over another screen");

        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
        app.tick();
        assert!(app.modal.is_some(), "prompt once back on the list");
    }

    #[test]
    fn a_silent_check_reporting_up_to_date_says_nothing() {
        let (_tmp, mut app) = test_app();
        let (tx, rx) = channel();
        tx.send(Ok(CheckOutcome::UpToDate {
            latest: update::CURRENT_VERSION.to_string(),
        }))
        .unwrap();
        app.update_check = Some(Task::new(rx));
        app.update_check_requested = false;

        app.tick();
        assert!(app.modal.is_none());
        assert!(app.message.is_none(), "a launch check stays quiet");
        assert!(app.error.is_none());
    }

    #[test]
    fn a_failed_launch_check_is_swallowed_but_a_requested_one_reports() {
        let (_tmp, mut app) = test_app();
        let (tx, rx) = channel();
        tx.send(Err("no network".to_string())).unwrap();
        app.update_check = Some(Task::new(rx));
        app.update_check_requested = false;
        app.tick();
        assert!(app.error.is_none(), "an unattended check must not pop up");

        let (tx, rx) = channel();
        tx.send(Err("no network".to_string())).unwrap();
        app.update_check = Some(Task::new(rx));
        app.update_check_requested = true;
        app.tick();
        assert!(
            app.error.as_deref().unwrap().contains("no network"),
            "a requested check reports its failure"
        );
    }

    #[test]
    fn check_now_reports_being_up_to_date() {
        let (_tmp, mut app) = test_app();
        goto_settings(&mut app);
        while app.settings.selected < config_editor::CHECK_ROW {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.message.as_deref().unwrap().contains("checking"));
        assert!(app.update_check.is_some(), "a check should be in flight");

        // Answer it, keeping the "requested" flag the key press set.
        let (tx, rx) = channel();
        tx.send(Ok(CheckOutcome::UpToDate {
            latest: "1.2.3".to_string(),
        }))
        .unwrap();
        app.update_check = Some(Task::new(rx));
        app.tick();
        assert!(
            app.message.as_deref().unwrap().contains("1.2.3"),
            "a requested check always reports: {:?}",
            app.message
        );
    }

    #[test]
    fn check_now_reoffers_a_previously_postponed_update() {
        let (_tmp, mut app) = test_app();
        app.update_available = Some(newer_release());
        app.tick();
        press(&mut app, KeyCode::Esc); // postpone
        assert!(app.modal.is_none());

        // Asking explicitly must offer it again rather than staying silent.
        app.start_update_check(true);
        let (tx, rx) = channel();
        tx.send(Ok(CheckOutcome::Available(newer_release())))
            .unwrap();
        app.update_check = Some(Task::new(rx));
        app.tick();
        assert!(app.modal.is_some(), "check-now re-offers the update");
    }

    #[test]
    fn accepting_the_update_starts_a_background_install() {
        let (_tmp, mut app) = test_app();
        app.update_available = Some(newer_release());
        app.tick();
        // Option 0 is "update and restart".
        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(app.view, View::Busy { .. }),
            "installing runs off the UI thread"
        );
        // The release carries no asset for this platform, so the install fails
        // and surfaces as an error rather than quitting or restarting.
        settle_busy(&mut app);
        assert!(app.error.is_some(), "a failed install must be reported");
        assert!(app.restart_exe.is_none(), "nothing to restart");
        assert!(!app.quit);
    }

    #[test]
    fn the_settings_tab_toggle_writes_the_global_config() {
        let (tmp, mut app) = test_app();
        let global = tmp.path().join("global.toml");
        goto_settings(&mut app);
        while app.settings.selected < config_editor::UPDATE_ROW {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter); // default -> on (writes)
        press(&mut app, KeyCode::Enter); // on -> off (writes)
        assert_eq!(app.settings.fields.auto_update_check, "false");

        let text = std::fs::read_to_string(&global).unwrap();
        assert!(text.contains("auto_update_check = false"), "{text}");
        // It belongs to the global config, not to this repo.
        let repo = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(!repo.contains("auto_update_check"), "{repo}");
    }

    #[test]
    fn double_ctrl_c_kills_a_stuck_setup() {
        let (_tmp, mut app) = test_app();
        app.ctx.config.setup.run = vec!["sleep 30".to_string(), "echo after".to_string()];
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "stuck");
        press(&mut app, KeyCode::Enter);
        wait_creating(&mut app, |lines, _| {
            lines.iter().any(|l| l.contains("running: sleep 30"))
        });

        ctrl_c(&mut app);
        assert!(
            app.message.as_deref().unwrap().contains("again to kill"),
            "first Ctrl+C should only arm the kill"
        );
        match &app.view {
            View::Creating { done, .. } => assert!(!done),
            _ => panic!("expected creating view"),
        }
        ctrl_c(&mut app);
        wait_creating(&mut app, |_, done| done);
        match &app.view {
            View::Creating { lines, .. } => {
                assert!(
                    lines.iter().any(|l| l.contains("aborted by user")),
                    "lines: {lines:?}"
                );
                assert!(
                    lines.iter().any(|l| l.contains("skipped: setup aborted")),
                    "lines: {lines:?}"
                );
            }
            _ => panic!("expected creating view"),
        }
        // The worktree itself is kept; only setup was aborted.
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == "stuck"));
    }

    #[test]
    fn typed_input_reaches_a_prompting_setup_command() {
        let (_tmp, mut app) = test_app();
        app.ctx.config.setup.run =
            vec!["echo ready && read line && test \"$line\" = hello".to_string()];
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "prompted");
        press(&mut app, KeyCode::Enter);
        wait_creating(&mut app, |lines, _| lines.iter().any(|l| l == "ready"));

        type_str(&mut app, "hello");
        press(&mut app, KeyCode::Enter);
        wait_creating(&mut app, |_, done| done);
        match &app.view {
            View::Creating { lines, .. } => {
                assert!(
                    lines.iter().any(|l| l.contains("❯ hello")),
                    "input should be echoed: {lines:?}"
                );
                assert!(
                    lines.iter().any(|l| l.starts_with("[ok] run ")),
                    "setup should succeed with the typed answer: {lines:?}"
                );
            }
            _ => panic!("expected creating view"),
        }
    }

    /// Renders every reachable view at two terminal sizes so layout math
    /// (popups, margins, clamps) can't panic at draw time.
    #[test]
    fn all_views_render_without_panicking() {
        for (w, h) in [(100u16, 30u16), (24, 8)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut draw = |app: &mut App| {
                terminal
                    .draw(|frame| crate::tui::ui::draw(frame, app))
                    .unwrap();
            };

            let (_tmp, mut app) = test_app();
            add_and_select_worktree(&mut app, "rendered");
            draw(&mut app); // list
            press(&mut app, KeyCode::Char('?'));
            draw(&mut app); // help
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Enter);
            draw(&mut app); // Changes tab
            press(&mut app, KeyCode::BackTab); // back to the Worktrees tab
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "rend");
            draw(&mut app); // create dialog: new-branch row plus checkout list
            press(&mut app, KeyCode::Tab);
            draw(&mut app); // base-branch picker floating over the dialog
            press(&mut app, KeyCode::Esc); // close picker
            press(&mut app, KeyCode::Esc); // close create dialog

            // Run-command prompt (no open_command configured).
            press(&mut app, KeyCode::Char('e'));
            type_str(&mut app, "echo hi");
            draw(&mut app); // run-command prompt
            press(&mut app, KeyCode::Esc);

            // Existing-directory prompt: creating a name that already exists.
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "rendered");
            press(&mut app, KeyCode::Enter);
            draw(&mut app); // directory-exists prompt (open/replace/cancel)
            press(&mut app, KeyCode::Esc);

            press(&mut app, KeyCode::Char('d'));
            draw(&mut app); // delete dialog
            press(&mut app, KeyCode::Down);
            draw(&mut app); // delete dialog, branch option selected
            press(&mut app, KeyCode::Esc);

            // Settings tab: navigating and mid-edit.
            goto_settings(&mut app);
            draw(&mut app);
            press(&mut app, KeyCode::Enter); // edit worktree_dir
            type_str(&mut app, "inside");
            draw(&mut app);
            press(&mut app, KeyCode::Esc); // cancel edit
            press(&mut app, KeyCode::Tab); // wraps around to the Worktrees tab

            // Creating view: while running (with typed input) and when done.
            app.ctx.config.setup.run = vec!["read line".to_string()];
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "drawn");
            press(&mut app, KeyCode::Enter);
            wait_creating(&mut app, |lines, _| {
                lines.iter().any(|l| l.contains("running:"))
            });
            type_str(&mut app, "typed");
            draw(&mut app); // running, input pending
            ctrl_c(&mut app);
            draw(&mut app); // kill armed warning
            ctrl_c(&mut app);
            wait_creating(&mut app, |_, done| done);
            draw(&mut app); // finished

            // Commit overlay with a changed file.
            std::fs::write(app.ctx.repo_root.join("scratch.txt"), "work\n").unwrap();
            app.refresh();
            app.selected = 0;
            press(&mut app, KeyCode::Char('c'));
            type_str(&mut app, "wip");
            draw(&mut app); // commit dialog
            press(&mut app, KeyCode::Esc);

            // Stash tab and its sub-modes.
            press(&mut app, KeyCode::Char('s'));
            draw(&mut app); // stash table (empty)
            press(&mut app, KeyCode::Char('s'));
            type_str(&mut app, "msg");
            draw(&mut app); // stash message input
            press(&mut app, KeyCode::Enter);
            settle(&mut app);
            draw(&mut app); // stash table with an entry
            press(&mut app, KeyCode::Char('x'));
            draw(&mut app); // drop confirm
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Tab); // Settings
            press(&mut app, KeyCode::Tab); // wraps around to Worktrees

            // Branches tab and its sub-modes.
            press(&mut app, KeyCode::Tab);
            draw(&mut app); // branch table
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "feat2");
            draw(&mut app); // create-branch input
            press(&mut app, KeyCode::Enter);
            settle(&mut app); // feat2 created
            draw(&mut app);
            press(&mut app, KeyCode::Char('d'));
            draw(&mut app); // delete confirm
            press(&mut app, KeyCode::Esc); // cancel delete
            press(&mut app, KeyCode::Tab); // back to Worktrees tab
            draw(&mut app);

            // Switch-branch picker (feat2 is available to switch onto).
            press(&mut app, KeyCode::Char('b'));
            draw(&mut app); // switch picker
            type_str(&mut app, "feat2");
            draw(&mut app); // filtered down to a match
            type_str(&mut app, "zzz");
            draw(&mut app); // filter with no matches
            press(&mut app, KeyCode::Esc); // clears the filter
            press(&mut app, KeyCode::Esc); // closes the picker

            // Log overlay.
            press(&mut app, KeyCode::Char('l'));
            draw(&mut app);
            press(&mut app, KeyCode::Esc);

            // Busy overlay (fetch with no remotes finishes quickly).
            press(&mut app, KeyCode::Char('f'));
            draw(&mut app); // busy spinner
            settle(&mut app);

            // Conflict resolver and its manual hunk editor.
            into_conflict_resolver(&mut app);
            draw(&mut app); // resolver with an undecided hunk
            press(&mut app, KeyCode::Char('e'));
            draw(&mut app); // manual hunk editor overlay (exercises the clamp)
            type_str(&mut app, "x");
            draw(&mut app);
            press(&mut app, KeyCode::Esc); // discard the edit
            press(&mut app, KeyCode::Char('x'));
            draw(&mut app); // abort confirmation over the resolver
            press(&mut app, KeyCode::Char('y')); // abort, back to the list

            // The setup wizard's screens.
            let (_tmp2, mut wizard_app) = test_app_uninitialized();
            draw(&mut wizard_app); // clone ask
            press(&mut wizard_app, KeyCode::Char('n'));
            draw(&mut wizard_app); // location presets
        }
    }

    #[test]
    fn background_create_completes_via_tick() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('n'));
        for c in "feat".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::Creating { .. }));

        // Wait for the worker thread, draining messages like the event loop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.tick();
            match &app.view {
                View::Creating { done: true, .. } => break,
                _ if std::time::Instant::now() > deadline => panic!("create timed out"),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::List));
        assert_eq!(app.worktrees.len(), 2);
        assert!(app.worktrees.iter().any(|w| w.name == "feat"));
        assert!(
            Path::new(
                &app.worktrees
                    .iter()
                    .find(|w| w.name == "feat")
                    .unwrap()
                    .path
            )
            .exists()
        );
    }

    /// Draws the app into an off-screen terminal, which is what records the
    /// click geometry (`tab_hits`, `preview_list`, `row_list`) the mouse
    /// handlers read. Tests that click must render first.
    fn render_app(app: &mut App, width: u16, height: u16) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, app))
            .unwrap();
    }

    /// Writes `count` changed files into `dir`, named so their sorted order is
    /// predictable.
    fn write_changed_files(dir: &Path, count: usize) {
        for i in 0..count {
            std::fs::write(dir.join(format!("f{i:02}.txt")), format!("{i}\n")).unwrap();
        }
    }

    /// The Worktrees tab's preview holds every changed file, not a capped
    /// slice, and reports the geometry needed to click one.
    #[test]
    fn worktree_preview_lists_every_changed_file() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 30);
        app.refresh();
        app.selected = 0;
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);

        let total = app.worktree_preview.len();
        assert!(total >= 30, "every change is previewed, got {total}");
        let rl = app.preview_list.expect("the preview records its geometry");
        assert_eq!(
            rl.len, total,
            "the whole list is clickable, not a capped slice"
        );
        assert!(
            (rl.inner.height as usize) < total,
            "this terminal is too short to show every row, so scrolling matters"
        );
        // Every file is reachable: the last row of the last page is the last file.
        assert_eq!(rl.offset, 0, "the preview starts at the top");
    }

    /// The wheel over the preview panel scrolls it, and the renderer clamps the
    /// offset so the viewport can't run off the end of the list.
    #[test]
    fn worktree_preview_scrolls_and_clamps() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 30);
        app.refresh();
        app.selected = 0;
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);
        let rl = app.preview_list.expect("preview geometry");
        let (col, row) = (rl.inner.x + 1, rl.inner.y + 1);
        let wheel = |app: &mut App, kind| {
            app.on_mouse(MouseEvent {
                kind,
                column: col,
                row,
                modifiers: KeyModifiers::empty(),
            });
        };

        wheel(&mut app, MouseEventKind::ScrollDown);
        render_app(&mut app, 100, 30);
        assert_eq!(app.preview_scroll, 3, "three rows per wheel notch");
        assert_eq!(
            app.preview_list.unwrap().offset,
            3,
            "the drawn rows start at the scroll offset"
        );

        // Far past the end: the renderer pins the last row to the bottom of the
        // panel instead of scrolling into empty space.
        for _ in 0..50 {
            press_shift(&mut app, KeyCode::Down);
        }
        render_app(&mut app, 100, 30);
        let visible = app.preview_list.unwrap().inner.height as usize;
        let last_page = app.worktree_preview.len() - visible;
        assert_eq!(app.preview_scroll, last_page, "clamped to the last page");

        wheel(&mut app, MouseEventKind::ScrollUp);
        assert_eq!(app.preview_scroll, last_page - 3, "the wheel scrolls back");
        for _ in 0..50 {
            press_shift(&mut app, KeyCode::Up);
        }
        assert_eq!(app.preview_scroll, 0, "back at the top");
    }

    /// Moving to another worktree resets the preview's scroll, so the panel
    /// isn't showing row 20 of a list that just changed underneath it.
    #[test]
    fn selecting_another_worktree_resets_the_preview_scroll() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 30);
        add_and_select_worktree(&mut app, "spare");
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        app.preview_scroll = 5;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.preview_scroll, 0);
    }

    /// Selection change kicks off a background status load instead of blocking
    /// the UI thread; a result whose index no longer matches is dropped.
    #[test]
    fn worktree_preview_loads_async_and_ignores_stale() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 5);
        add_and_select_worktree(&mut app, "spare");
        let main = app.worktrees.iter().position(|w| w.is_main).unwrap();
        let spare = app
            .worktrees
            .iter()
            .position(|w| w.name == "spare")
            .unwrap();

        app.selected = main;
        app.ensure_worktree_preview();
        assert!(
            app.preview_loading(),
            "first ensure starts a background load"
        );
        settle_preview(&mut app);
        assert_eq!(app.preview_for, Some(main));
        let main_count = app.worktree_preview.len();
        assert!(main_count >= 5, "main worktree's changes land");

        // Start a load for spare, then jump back to main before it lands.
        app.selected = spare;
        app.ensure_worktree_preview();
        assert!(app.preview_loading(), "spare load is in flight");
        app.selected = main;
        app.ensure_worktree_preview();
        assert!(
            !app.preview_loading(),
            "cached main preview is shown again immediately"
        );
        assert_eq!(app.preview_for, Some(main));

        // Drain the orphaned spare result; it must not replace main's cache.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.preview_pending.is_some() {
            app.poll_preview_load();
            assert!(
                std::time::Instant::now() < deadline,
                "orphaned preview load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(app.preview_for, Some(main));
        assert_eq!(
            app.worktree_preview.len(),
            main_count,
            "stale spare status was ignored"
        );
    }

    /// Clicking a tab label switches to that tab and runs its entry loader.
    #[test]
    fn clicking_a_tab_switches_to_it() {
        let (_tmp, mut app) = test_app();
        app.refresh();
        app.selected = 0;
        render_app(&mut app, 100, 30);

        let (rect, _) = *app
            .tab_hits
            .iter()
            .find(|(_, t)| *t == Tab::Branches)
            .expect("the tab bar records a hit box per tab");
        click(&mut app, rect.x + 1, rect.y);
        assert_eq!(app.tab, Tab::Branches);
        assert!(
            app.branches_loading(),
            "switching to Branches kicks off a background branch-list load"
        );
        settle_branches(&mut app);
        assert!(
            !app.branches.is_empty(),
            "switching to Branches loads the branch list"
        );

        render_app(&mut app, 100, 30);
        let (rect, _) = *app
            .tab_hits
            .iter()
            .find(|(_, t)| *t == Tab::Worktrees)
            .unwrap();
        click(&mut app, rect.x + 1, rect.y);
        assert_eq!(app.tab, Tab::Worktrees);
    }

    /// A modal covers the tab bar, so a click there belongs to the modal.
    #[test]
    fn tab_clicks_are_ignored_while_a_modal_is_up() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "spare");
        press(&mut app, KeyCode::Char('d')); // delete confirm modal
        assert!(app.modal.is_some());
        render_app(&mut app, 100, 30);
        assert!(app.tab_hits.is_empty(), "the modal owns every click");
        click(&mut app, 12, 1);
        assert_eq!(app.tab, Tab::Worktrees, "the tab did not change");
    }

    /// Clicking a changed file in the preview opens the Changes tab with the
    /// cursor already on that file.
    #[test]
    fn clicking_a_preview_file_opens_it_on_the_changes_tab() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 12);
        app.refresh();
        app.selected = 0;
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);

        // Scroll down a page so the click also proves the offset is applied.
        app.preview_scroll = 4;
        render_app(&mut app, 100, 30);
        let rl = app.preview_list.expect("preview geometry");
        // Third visible row -> the sixth file in the list.
        let want = app.worktree_preview[rl.offset + 2].path.clone();
        click(&mut app, rl.inner.x + 1, rl.inner.y + 2);
        settle_diff(&mut app);

        assert_eq!(app.tab, Tab::Changes);
        assert_eq!(app.changes.name, app.worktrees[0].name);
        let idx = current_file_index(&app.changes.rows, app.changes.selected)
            .expect("the cursor sits on a file row, not a folder");
        assert_eq!(app.changes.files[idx].path, want);
    }

    /// A clicked file inside a collapsed folder still gets the cursor: its
    /// ancestors are expanded so it has a row to land on.
    #[test]
    fn clicking_a_preview_file_expands_its_collapsed_folder() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("pkg/deep")).unwrap();
        std::fs::write(root.join("pkg/deep/a.txt"), "a\n").unwrap();
        app.refresh();
        app.selected = 0;
        app.collapsed_folders.insert("pkg/".to_string());
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);

        let rl = app.preview_list.expect("preview geometry");
        let idx = app
            .worktree_preview
            .iter()
            .position(|e| e.path == "pkg/deep/a.txt")
            .expect("the new file is previewed");
        click(&mut app, rl.inner.x + 1, rl.inner.y + idx as u16);
        settle_diff(&mut app);

        assert_eq!(app.tab, Tab::Changes);
        let file = current_file_index(&app.changes.rows, app.changes.selected)
            .expect("the cursor sits on a file row");
        assert_eq!(app.changes.files[file].path, "pkg/deep/a.txt");
    }

    /// Opens the Changes tab on the main worktree with one changed file, cursor
    /// already on it, and the frame drawn so click geometry is recorded.
    fn changes_tab_with_one_file(app: &mut App) -> String {
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("hello.txt"), "hi\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(app, Tab::Changes);
        settle_diff(app);
        let idx = current_file_index(&app.changes.rows, app.changes.selected)
            .expect("the cursor sits on the changed file");
        let path = app.changes.files[idx].path.clone();
        render_app(app, 100, 30);
        path
    }

    /// Shift+U discards every uncommitted change in the worktree: tracked
    /// files reset to HEAD, untracked files removed, after confirming.
    #[test]
    fn shift_u_discards_all_changes_after_confirm() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add tracked"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::write(root.join("tracked.txt"), "modified\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(&mut app, Tab::Changes);
        settle_diff(&mut app);

        press(&mut app, KeyCode::Char('U'));
        assert!(
            matches!(app.modal, Some(Modal::Confirm { .. })),
            "expected a confirmation before discarding everything"
        );
        press(&mut app, KeyCode::Char('y'));

        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "original\n"
        );
        assert!(!root.join("new.txt").exists());
        assert_eq!(
            app.message.as_deref(),
            Some(&*format!("discarded all changes in '{}'", app.changes.name))
        );
    }

    /// Enter on a file row hands the worktree's copy of the file to the OS.
    #[test]
    fn enter_on_a_changed_file_opens_it_with_the_default_app() {
        let (_tmp, mut app) = test_app();
        let path = changes_tab_with_one_file(&mut app);
        let root = ops::path(&app.ctx, &app.changes.name).unwrap();
        platform::take_recorded();

        press(&mut app, KeyCode::Enter);
        let want = format!("open {}", Path::new(&root).join(&path).display());
        assert_eq!(platform::take_recorded(), vec![want]);
        assert_eq!(app.message.as_deref(), Some(&*format!("opened '{path}'")));
    }

    /// Enter on a folder row still toggles it instead of opening anything.
    #[test]
    fn enter_on_a_folder_row_toggles_it_and_opens_nothing() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/a.txt"), "a\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(&mut app, Tab::Changes);
        settle_diff(&mut app);
        // Put the cursor on the "pkg/" folder row.
        app.changes.selected = app
            .changes
            .rows
            .iter()
            .position(|r| matches!(r, DiffRow::Folder { .. }))
            .expect("a folder row exists");
        platform::take_recorded();

        press(&mut app, KeyCode::Enter);
        assert!(
            app.collapsed_folders.contains("pkg/"),
            "Enter collapsed the folder"
        );
        assert!(
            platform::take_recorded().is_empty(),
            "a folder is not handed to the OS"
        );
    }

    /// A deleted file has nothing to open, and says so instead of failing.
    #[test]
    fn opening_a_deleted_file_reports_it_is_gone() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("gone.txt"), "x\n").unwrap();
        // Commit it, then delete it so it shows up as a deleted change.
        Command::new("git")
            .args(["add", "gone.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add gone.txt"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(&mut app, Tab::Changes);
        settle_diff(&mut app);
        app.changes.selected = app
            .changes
            .rows
            .iter()
            .position(|r| match r {
                DiffRow::File { index, .. } => app.changes.files[*index].path == "gone.txt",
                DiffRow::Folder { .. } => false,
            })
            .expect("the deleted file has a row");
        platform::take_recorded();

        press(&mut app, KeyCode::Enter);
        assert!(platform::take_recorded().is_empty(), "nothing was opened");
        assert_eq!(
            app.message.as_deref(),
            Some("'gone.txt' no longer exists in the worktree")
        );
    }

    /// Two quick clicks on the same file row open it; a single click only moves
    /// the cursor, and a slow pair is two singles.
    #[test]
    fn double_clicking_a_changed_file_opens_it() {
        let (_tmp, mut app) = test_app();
        let path = changes_tab_with_one_file(&mut app);
        let rl = app.row_list.expect("the file list records its geometry");
        let (col, row) = (rl.inner.x + 1, rl.inner.y);
        platform::take_recorded();

        click(&mut app, col, row);
        assert!(
            platform::take_recorded().is_empty(),
            "one click only moves the cursor"
        );
        click(&mut app, col, row);
        assert_eq!(platform::take_recorded().len(), 1, "the pair opened it");
        assert_eq!(app.message.as_deref(), Some(&*format!("opened '{path}'")));

        // A third click starts a new pair rather than firing again.
        click(&mut app, col, row);
        assert!(
            platform::take_recorded().is_empty(),
            "the pair was consumed"
        );
        // A click on a different cell doesn't pair with the one before it.
        click(&mut app, col + 1, row);
        assert!(platform::take_recorded().is_empty(), "different cell");
    }

    /// Clicking the diff panel's path title copies the path, and doesn't count
    /// towards a double click that would also open the file.
    #[test]
    fn clicking_the_diff_path_copies_it() {
        let (_tmp, mut app) = test_app();
        let path = changes_tab_with_one_file(&mut app);
        let hit = app
            .diff_path_hit
            .expect("the diff panel records its path title");
        platform::take_recorded();

        click(&mut app, hit.x + 1, hit.y);
        assert_eq!(platform::take_recorded(), vec![format!("copy {path}")]);
        assert_eq!(
            app.message.as_deref(),
            Some(&*format!("copied '{path}' to the clipboard"))
        );

        // Twice in a row copies twice and never opens the file.
        click(&mut app, hit.x + 1, hit.y);
        assert_eq!(platform::take_recorded(), vec![format!("copy {path}")]);
    }

    /// A folder row has no file path, so the title is not a copy target.
    #[test]
    fn the_diff_path_is_not_clickable_on_a_folder_row() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/a.txt"), "a\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(&mut app, Tab::Changes);
        settle_diff(&mut app);
        app.changes.selected = app
            .changes
            .rows
            .iter()
            .position(|r| matches!(r, DiffRow::Folder { .. }))
            .expect("a folder row exists");
        render_app(&mut app, 100, 30);
        assert!(app.diff_path_hit.is_none());
    }

    /// Shift+←/→ (and H/L, which arrive as plain capitals on terminals that
    /// drop the modifier) pan the Changes-tab diff sideways, and switching
    /// files sends the offset back to the left edge.
    #[test]
    fn shift_arrows_scroll_the_changes_diff_horizontally() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("narrow.txt"), "hi\n").unwrap();
        // A long line so the offset has somewhere to go: it is clamped to the
        // longest line in the diff on screen.
        std::fs::write(root.join("wide.txt"), format!("{}\n", "x".repeat(200))).unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        goto_tab(&mut app, Tab::Changes);
        select_diff_file(&mut app, "wide.txt");
        assert_eq!(app.changes.h_scroll, 0);

        press_shift(&mut app, KeyCode::Right);
        assert_eq!(app.changes.h_scroll, 8);
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(app.changes.h_scroll, 16);
        press_shift(&mut app, KeyCode::Left);
        assert_eq!(app.changes.h_scroll, 8);
        press(&mut app, KeyCode::Char('H'));
        assert_eq!(app.changes.h_scroll, 0);

        press(&mut app, KeyCode::Char('L'));
        assert_eq!(app.changes.h_scroll, 8);
        let row = app.changes.selected;
        press(&mut app, KeyCode::Up);
        assert_ne!(app.changes.selected, row, "the cursor moved off wide.txt");
        settle_diff(&mut app);
        assert_eq!(app.changes.h_scroll, 0, "a file switch resets the offset");
    }

    /// Enter on the Stash tab opens a read-only browser for the entry's files,
    /// loading each stashed diff off-thread like the commit browser does.
    #[test]
    fn stash_enter_browses_the_stashed_files() {
        let (_tmp, mut app) = test_app();
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        // Commit everything, `.wtm.toml` included: stashing sweeps up untracked
        // files, and an extra entry would sort ahead of f.txt in the browser.
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "add f"]);
        // A long replacement line, so the browser's horizontal offset has room.
        std::fs::write(root.join("f.txt"), format!("{}\n", "y".repeat(200))).unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();

        press(&mut app, KeyCode::Char('s')); // the Stash tab
        press(&mut app, KeyCode::Char('s')); // stash the change
        press(&mut app, KeyCode::Enter); // no message
        settle(&mut app);
        assert_eq!(app.stash_entries.len(), 1);

        press(&mut app, KeyCode::Enter);
        settle_browser_diff(&mut app);
        match &app.view {
            View::StashDiff {
                index,
                label,
                files,
                content,
                ..
            } => {
                assert_eq!(*index, 0);
                assert!(label.starts_with("stash@{0}"), "{label}");
                assert!(files.iter().any(|f| f.path == "f.txt"), "{files:?}");
                assert!(content.contains("yyy"), "the stashed diff: {content}");
            }
            _ => panic!("expected the stash browser"),
        }

        press_shift(&mut app, KeyCode::Right);
        match &app.view {
            View::StashDiff { h_scroll, .. } => assert_eq!(*h_scroll, 8),
            _ => panic!("expected the stash browser"),
        }

        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
        assert_eq!(app.tab, Tab::Stash, "back on the tab it was opened from");
    }

    /// p/P on the Changes tab act on the worktree whose diffs are open, which
    /// need not be the one the Worktrees tab has under its cursor.
    #[test]
    fn changes_tab_pull_and_push_use_the_open_worktree() {
        let (_tmp, mut app) = test_app();
        add_and_select_worktree(&mut app, "feature");
        goto_tab(&mut app, Tab::Changes);
        assert_eq!(app.changes.name, "feature");
        // Park the Worktrees-tab cursor elsewhere: it must not be consulted.
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();

        press(&mut app, KeyCode::Char('p'));
        match &app.view {
            View::Busy { label, then, .. } => {
                assert_eq!(label, "pulling feature…");
                assert!(matches!(then, BusyThen::Pull { name } if name == "feature"));
            }
            _ => panic!("expected the busy overlay"),
        }
        settle(&mut app);
        // No upstream in a test repo, so the pull reports an error; clear it or
        // the next key press would only dismiss the popup.
        app.error = None;
        assert_eq!(app.tab, Tab::Changes);
        assert_eq!(app.changes.name, "feature");

        press(&mut app, KeyCode::Char('P'));
        match &app.view {
            View::Busy { label, .. } => assert_eq!(label, "pushing feature…"),
            _ => panic!("expected the busy overlay"),
        }
        settle(&mut app);
    }

    /// An app whose repo asks for the three-panel Worktrees layout, with one
    /// changed file so the bottom panels have something to show.
    fn three_panel_app() -> (tempfile::TempDir, App) {
        let (tmp, mut app) = test_app();
        app.ctx.config.worktrees_layout = Some(WorktreesLayout::ThreePanel);
        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        // The renderer decides the effective layout, so draw once before the
        // key handlers are asked about it.
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);
        (tmp, app)
    }

    /// Three-panel layout with a clean main worktree (no uncommitted changes),
    /// so the bottom area should show the branch commit list.
    fn three_panel_clean_app() -> (tempfile::TempDir, App) {
        let (tmp, mut app) = test_app();
        app.ctx.config.worktrees_layout = Some(WorktreesLayout::ThreePanel);
        let root = app.ctx.repo_root.clone();
        // `.wtm.toml` is written after the seed commit, so commit it (and a
        // second empty commit) before treating the worktree as clean.
        git(&root, &["add", ".wtm.toml"]);
        git(&root, &["commit", "-m", "wtm config"]);
        git(&root, &["commit", "--allow-empty", "-m", "second"]);
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        render_app(&mut app, 100, 30);
        (tmp, app)
    }

    /// Enter hands the keyboard to the changed-file panel and q/Esc gives it
    /// back, so the app still only quits from the worktree list.
    #[test]
    fn three_panel_enter_focuses_the_files_panel_and_esc_returns() {
        let (_tmp, mut app) = three_panel_app();
        assert!(app.three_panel, "the configured layout is in force");
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
        assert_eq!(app.tab, Tab::Worktrees, "no tab switch, just focus");
        assert_eq!(app.changes.name, app.worktrees[app.selected].name);

        // q from the file panel returns focus instead of quitting.
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert!(!app.quit);
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);

        // A q right after back is ignored (debounce for buffered keys). Clear
        // the window so an intentional quit from the list still works.
        app.ignore_quit_until = None;
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    /// The file panel gets the Changes-tab keys: the cursor moves there, not
    /// over the worktree list.
    #[test]
    fn three_panel_files_focus_moves_the_file_cursor() {
        let (_tmp, mut app) = three_panel_app();
        let root = app.ctx.repo_root.clone();
        write_changed_files(&root, 5);
        app.refresh();
        app.selected = app.worktrees.iter().position(|w| w.is_main).unwrap();
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);

        press(&mut app, KeyCode::Enter);
        let worktree_cursor = app.selected;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.changes.selected, 1, "the file cursor moved");
        assert_eq!(app.selected, worktree_cursor, "the worktree cursor did not");
    }

    /// The Changes tab is folded into the Worktrees tab, so it is skipped by
    /// the Tab key, left out of the tab bar, and redirected to the file panel.
    #[test]
    fn three_panel_hides_the_changes_tab() {
        let (_tmp, mut app) = three_panel_app();
        assert!(app.tab_hidden(Tab::Changes));

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches, "the cycle steps past Changes");
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.tab, Tab::Worktrees, "and back the other way");

        render_app(&mut app, 100, 30);
        assert!(
            !app.tab_hits.iter().any(|(_, tab)| *tab == Tab::Changes),
            "the tab bar doesn't offer it either"
        );

        // Asking for it anyway (a click, or code calling `select_tab`) lands on
        // the file panel of the Worktrees tab.
        goto_tab(&mut app, Tab::Changes);
        assert_eq!(app.tab, Tab::Worktrees);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
    }

    /// Moving the worktree cursor swaps the bottom panels to that worktree,
    /// off the UI thread via the same status load the preview uses.
    #[test]
    fn three_panel_changes_follow_the_worktree_cursor() {
        let (_tmp, mut app) = three_panel_app();
        add_and_select_worktree(&mut app, "feature");
        let feature_root = app.worktrees[app.selected].path.clone();
        std::fs::write(Path::new(&feature_root).join("b.txt"), "b\n").unwrap();
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        assert_eq!(app.changes.name, "feature");
        assert!(app.changes.files.iter().any(|f| f.path == "b.txt"));

        let main = app.worktrees.iter().position(|w| w.is_main).unwrap();
        app.selected = main;
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        assert_eq!(app.changes.name, app.worktrees[main].name);
        assert!(app.changes.files.iter().any(|f| f.path == "a.txt"));
    }

    /// A click on either list takes the focus, the mouse equivalent of
    /// Enter and q.
    #[test]
    fn three_panel_clicks_steal_focus() {
        let (_tmp, mut app) = three_panel_app();
        let files = app.files_list.expect("the file panel records its geometry");
        click(&mut app, files.inner.x + 1, files.inner.y);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);

        render_app(&mut app, 100, 30);
        let list = app
            .row_list
            .expect("the worktree list records its geometry");
        click(&mut app, list.inner.x + 1, list.inner.y + list.header);
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);
    }

    /// Too short a terminal falls back to two panels, which brings the Changes
    /// tab back so the diff is still reachable.
    #[test]
    fn three_panel_falls_back_in_a_short_terminal() {
        let (_tmp, mut app) = three_panel_app();
        render_app(&mut app, 100, 12);
        assert!(!app.three_panel);
        assert!(!app.tab_hidden(Tab::Changes));
        render_app(&mut app, 100, 30);
        assert!(app.three_panel, "and comes back when there is room again");
    }

    /// Nothing about the two-panel path changes: it is still the default, Enter
    /// opens the Changes tab, and that tab stays in the cycle.
    #[test]
    fn two_panel_is_the_default_layout() {
        let (_tmp, mut app) = test_app();
        assert_eq!(
            app.ctx.config.worktrees_layout(),
            WorktreesLayout::TwoPanel,
            "an empty config means two panels"
        );
        render_app(&mut app, 100, 30);
        assert!(!app.three_panel);
        assert!(!app.tab_hidden(Tab::Changes));

        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Changes, "the Changes tab is in the cycle");
        app.select_tab(Tab::Worktrees);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.tab, Tab::Changes, "Enter opens the Changes tab");
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);
    }

    /// Saving on the Settings tab reloads the merged config, so the next frame
    /// already draws the newly chosen layout: no restart to switch. A stale
    /// repo-level override is cleared so Settings (global) actually wins.
    #[test]
    fn saving_settings_switches_the_layout_without_a_restart() {
        let (_tmp, mut app) = test_app();
        let global = app.settings.global_config.clone().unwrap();
        std::fs::write(&global, "# test global\n").unwrap();

        // Stale repo override: this is what made Settings saves look like no-ops.
        let file = app.ctx.repo_root.join(".wtm.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}\nworktrees_layout = \"two_panel\"\n")).unwrap();
        app.ctx.config = crate::config::Config::merge(
            crate::config::FileConfig::load(&global).unwrap(),
            crate::config::FileConfig::load(&file).unwrap(),
        );
        render_app(&mut app, 100, 30);
        assert!(!app.three_panel);

        goto_settings(&mut app);
        assert_eq!(
            app.settings.fields.worktrees_layout, "two_panel",
            "editor shows the effective (repo) value, not a masked global"
        );
        while app.settings.selected < config_editor::LAYOUT_ROW {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter); // two_panel → three_panel (writes immediately)
        assert_eq!(app.settings.fields.worktrees_layout, "three_panel");

        // Settings writes the UI pref to its dedicated global path (tests point
        // that at a temp file) and clears the repo override. Re-merge from those
        // two files: production keeps `settings.global_config` in sync with
        // `Config::load`'s path via `global_config_path()`.
        assert_eq!(
            crate::config::FileConfig::load(&global)
                .unwrap()
                .worktrees_layout,
            Some(WorktreesLayout::ThreePanel)
        );
        let repo_text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !repo_text.contains("worktrees_layout"),
            "repo override must be cleared: {repo_text}"
        );
        app.ctx.config = crate::config::Config::merge(
            crate::config::FileConfig::load(&global).unwrap(),
            crate::config::FileConfig::load(&file).unwrap(),
        );
        assert_eq!(
            app.ctx.config.worktrees_layout(),
            WorktreesLayout::ThreePanel
        );
        assert_eq!(
            app.ctx.config.worktrees_layout_source,
            crate::config::Source::Global
        );

        app.select_tab(Tab::Worktrees);
        render_app(&mut app, 100, 30);
        assert!(app.three_panel, "the next frame draws three panels");
        assert!(app.tab_hidden(Tab::Changes));
    }

    /// A clean worktree fills the three-panel bottom with that branch's commit
    /// history instead of an empty changes list.
    #[test]
    fn three_panel_clean_worktree_shows_commit_list() {
        let (_tmp, app) = three_panel_clean_app();
        assert!(app.worktree_preview.is_empty());
        assert!(app.changes.files.is_empty());
        assert!(app.showing_worktree_commits());
        let panel = app
            .worktree_commits
            .as_ref()
            .expect("clean worktree shows commits");
        assert!(!panel.lines.is_empty(), "branch history should load");
        assert_eq!(panel.name, app.worktrees[app.selected].name);
        assert_eq!(
            Some(panel.branch.as_str()),
            app.worktrees[app.selected].branch.as_deref()
        );
        assert!(panel.from_branch);
        assert!(
            app.files_list.is_some(),
            "the commit list records click geometry"
        );
    }

    /// A dirty worktree keeps the existing changes + diff bottom panels.
    #[test]
    fn three_panel_dirty_worktree_keeps_changes_panel() {
        let (_tmp, app) = three_panel_app();
        assert!(!app.showing_worktree_commits());
        assert!(app.changes.files.iter().any(|f| f.path == "a.txt"));
    }

    /// With the commit list focused, j/k move the commit cursor and Enter
    /// opens the commit file browser (BranchCommits-style).
    #[test]
    fn three_panel_commits_navigate_and_open_diff() {
        let (_tmp, mut app) = three_panel_clean_app();
        let worktree_cursor = app.selected;
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
        let start = app.worktree_commits.as_ref().unwrap().selected;
        press(&mut app, KeyCode::Down);
        let after = app.worktree_commits.as_ref().unwrap().selected;
        assert_ne!(after, start, "j/↓ moves to another commit");
        assert_eq!(app.selected, worktree_cursor, "worktree cursor stays");

        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(app.view, View::CommitDiff { .. }),
            "Enter opens the commit browser"
        );
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
        assert!(app.worktree_commits.is_some());
    }

    /// The commits panel shows the *selected worktree's* history, so the
    /// worktree keymap (pull/push/fetch/update/…) has to work from there too,
    /// without shadowing the panel's own keys.
    #[test]
    fn three_panel_commits_panel_runs_worktree_commands() {
        let (_tmp, mut app) = three_panel_clean_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
        assert!(app.worktree_commits.is_some());

        // The panel's own binding still wins over the worktree list's.
        let before = app.log_mode;
        press(&mut app, KeyCode::Char('t'));
        assert_ne!(app.log_mode, before, "t still toggles the log mode");

        // A key the panel doesn't bind falls through to the worktree keymap.
        press(&mut app, KeyCode::Char('p'));
        assert!(
            matches!(app.view, View::Busy { .. }),
            "p must start a pull from the commits panel"
        );
        settle(&mut app);
        app.error = None;

        press(&mut app, KeyCode::Char('f'));
        assert!(
            matches!(app.view, View::Busy { .. }),
            "f must fetch from the commits panel"
        );
        settle(&mut app);
        app.error = None;

        // Dialogs open from here as well.
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.view, View::Create { .. }), "n opens create");
    }

    /// The changed-file panel gets the worktree commands its own keymap leaves
    /// unbound, without disturbing the ones it does bind (`u` reverts a file,
    /// it does not update the worktree).
    #[test]
    fn three_panel_files_panel_reaches_unbound_worktree_keys() {
        let (_tmp, mut app) = three_panel_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
        assert!(app.worktree_commits.is_none(), "dirty worktree: file panel");

        press(&mut app, KeyCode::Char('f'));
        assert!(
            matches!(app.view, View::Busy { .. }),
            "f must fetch from the file panel"
        );
        settle(&mut app);
        app.error = None;

        press(&mut app, KeyCode::Char('u'));
        assert!(
            !matches!(app.view, View::Busy { .. }),
            "u still means revert-file here, not update"
        );
    }

    /// Dirty ↔ clean flips swap the three-panel bottom between changes and
    /// commits without leaving the Worktrees tab.
    #[test]
    fn three_panel_switches_between_changes_and_commits() {
        let (_tmp, mut app) = three_panel_clean_app();
        assert!(app.worktree_commits.is_some());

        let root = app.ctx.repo_root.clone();
        std::fs::write(root.join("dirty.txt"), "x\n").unwrap();
        app.refresh_diff();
        settle_status_refresh(&mut app);
        assert!(
            app.worktree_commits.is_none(),
            "dirty status drops the commit list"
        );
        assert!(app.changes.files.iter().any(|f| f.path == "dirty.txt"));

        std::fs::remove_file(root.join("dirty.txt")).unwrap();
        app.refresh_diff();
        settle_status_refresh(&mut app);
        settle_commits(&mut app);
        assert!(
            app.worktree_commits.is_some(),
            "clean status brings the commit list back"
        );
        assert!(app.changes.files.is_empty());
    }

    /// Two-panel layout never fills `worktree_commits`; clean means empty
    /// Changes tab / preview, not an in-tab commit list.
    #[test]
    fn two_panel_clean_worktree_does_not_show_commit_panel() {
        let (_tmp, mut app) = test_app();
        render_app(&mut app, 100, 30);
        settle_preview(&mut app);
        assert!(!app.three_panel);
        assert!(app.worktree_commits.is_none());
    }

    /// Moving the worktree cursor kicks off a background commits load rather
    /// than blocking the UI thread on `git log --graph`.
    #[test]
    fn three_panel_selection_loads_commits_in_the_background() {
        let (_tmp, mut app) = three_panel_clean_app();
        assert!(app.showing_worktree_commits());
        add_and_select_worktree(&mut app, "other");
        // Make the new worktree clean with its own history.
        let other = app.worktrees.iter().find(|w| w.name == "other").unwrap();
        let path = PathBuf::from(&other.path);
        git(&path, &["commit", "--allow-empty", "-m", "on other"]);
        app.refresh();
        app.selected = app
            .worktrees
            .iter()
            .position(|w| w.name == "other")
            .unwrap();
        app.preview_for = None;
        app.worktree_commits = None;
        app.commits_pending = None;

        app.ensure_worktree_preview();
        assert!(app.preview_loading());
        // Preview landing starts the commits load without filling the panel yet.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.preview_pending.is_some() {
            app.poll_preview_load();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            app.commits_pending.is_some() || app.worktree_commits.is_some(),
            "clean worktree should start or finish a commits load"
        );
        // If still pending, the panel must not already show the previous tree.
        if app.commits_pending.is_some() {
            assert!(
                app.worktree_commits
                    .as_ref()
                    .is_none_or(|p| p.name == "other"),
                "must not flash another worktree's commits"
            );
        }
        settle_commits(&mut app);
        assert!(app.showing_worktree_commits());
        assert_eq!(app.worktree_commits.as_ref().unwrap().name, "other");
    }

    /// q from the commits panel only moves focus back to the list; it does not
    /// reload the commit history.
    #[test]
    fn three_panel_q_returns_focus_without_reloading_commits() {
        let (_tmp, mut app) = three_panel_clean_app();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.worktrees_focus, WorktreesFocus::Files);
        let before = app.worktree_commits.as_ref().unwrap().lines.len();
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.worktrees_focus, WorktreesFocus::List);
        assert!(app.commits_pending.is_none());
        assert_eq!(
            app.worktree_commits.as_ref().unwrap().lines.len(),
            before,
            "q must not reload the commit list"
        );
    }

    /// Opening the Stash tab starts a background list load so Tab cycling never
    /// blocks on `git stash list`.
    #[test]
    fn stash_tab_loads_in_the_background() {
        let (_tmp, mut app) = test_app();
        app.refresh();
        app.selected = 0;
        // Call the loader directly so press()'s settle doesn't hide the pending state.
        app.open_stash_tab();
        assert_eq!(app.tab, Tab::Stash);
        assert!(app.stash_loading(), "stash list must load off-thread");
        settle_stash(&mut app);
        assert!(!app.stash_loading());
    }

    /// With more worktrees than the three-panel list can show, overflow arrows
    /// appear on the left: ▼ when rows sit below the viewport, ▲ when scrolled.
    #[test]
    fn three_panel_worktree_list_shows_scroll_arrows() {
        let (_tmp, mut app) = three_panel_app();
        for i in 0..6 {
            add_and_select_worktree(&mut app, &format!("extra-{i}"));
        }
        assert!(
            app.worktrees.len() > 4,
            "need more than the four visible rows"
        );

        // At the top: only a down arrow.
        app.selected = 0;
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app))
            .unwrap();
        let top = terminal.backend().to_string();
        assert!(top.contains('▼'), "more below at the top: {top}");
        assert!(!top.contains('▲'), "nothing above at the top: {top}");

        // Mid-list: both arrows when the selection has scrolled the viewport.
        app.selected = app.worktrees.len() - 1;
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app))
            .unwrap();
        let bottom = terminal.backend().to_string();
        assert!(bottom.contains('▲'), "more above at the bottom: {bottom}");
    }
}
