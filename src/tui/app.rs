//! TUI application state and key handling.

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use super::config_editor::{ConfigEditor, EditorOutcome};
use super::setup::{self, SetupWizard, WizardOutcome};
use crate::git::{LogEntry, StashEntry, StatusEntry};
use crate::ops::{self, BranchListItem, Ctx, SetupControl, WorktreeInfo};
use crate::settings::ConfigDraft;

/// A single-line text field with a movable insertion cursor. `cursor` is a
/// character index in `0..=value.chars().count()`, so `←/→`, Home/End, and
/// mid-string insert/delete all work instead of edit-at-the-end only.
#[derive(Default, Clone)]
pub struct TextInput {
    pub value: String,
    pub cursor: usize,
}

impl TextInput {
    fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// Byte offset of character index `idx`, for slicing `value`.
    fn byte_at(&self, idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(idx)
            .map(|(b, _)| b)
            .unwrap_or(self.value.len())
    }

    fn insert(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.value.insert(b, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.byte_at(self.cursor - 1);
            let end = self.byte_at(self.cursor);
            self.value.replace_range(start..end, "");
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let start = self.byte_at(self.cursor);
            let end = self.byte_at(self.cursor + 1);
            self.value.replace_range(start..end, "");
        }
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
}

/// Message from the background create thread.
pub enum CreateMsg {
    Progress(String),
    Done(Result<crate::ops::CreateResult, String>),
}

/// How often the diff view recomputes itself to pick up outside edits.
const DIFF_REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

/// How long a status/error message stays on screen before auto-clearing.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(4);

/// Which screen/overlay is active.
pub enum View {
    List,
    /// Per-file changes browser for one worktree: a list of changed files on
    /// the left and the selected file's diff on the right. Files can be marked
    /// for commit, stashed, or reverted from here. Re-runs on a throttled timer
    /// (to catch edits made outside the app) and on `r`.
    Diff {
        name: String,
        /// Changed files, parallel with `marked`.
        files: Vec<StatusEntry>,
        /// Whether each file is selected for commit; defaults to all true.
        marked: Vec<bool>,
        /// Folder-tree rows derived from `files`, rebuilt whenever `files`
        /// changes. The cursor (`selected`) indexes into this, not `files`.
        rows: Vec<DiffRow>,
        /// Cursor into `rows`.
        selected: usize,
        /// Diff text for the file under the cursor (empty on a folder row).
        content: String,
        scroll: u16,
        /// When the diff was last recomputed, used to throttle auto-refresh.
        last_refresh: Instant,
        /// True while confirming a revert of the highlighted file.
        confirm_revert: bool,
        /// Present while choosing what to add to `.gitignore` for the
        /// highlighted file or folder (the exact path vs. a glob pattern).
        ignore_prompt: Option<IgnorePrompt>,
    },
    /// New-worktree dialog. Row 0 creates a new branch (named in `name`) off
    /// `base`; the rows below check out an existing branch. This keeps the two
    /// distinct actions clearly separated.
    Create {
        /// Name of the new branch (and its worktree folder). Row 0 only.
        name: TextInput,
        /// Local branches not checked out anywhere: the checkout options.
        branches: Vec<String>,
        /// Every local branch, for choosing a base to branch off of.
        all_branches: Vec<String>,
        /// Base ref a new branch is created from (defaults to the main branch).
        base: String,
        /// 0 = new branch; 1..=branches.len() = check out branches[selected-1].
        selected: usize,
        /// True when the `[ Base: … ⌄ ]` button is focused (via Tab from the
        /// new-branch row), so Enter/Space opens the base picker instead of
        /// creating. Only meaningful while `selected == 0`.
        base_focus: bool,
        /// Some(idx) while picking the base branch: index into `all_branches`.
        base_pick: Option<usize>,
    },
    /// The target directory for a create already exists; offer to open it (when
    /// it is a worktree), replace it, or cancel.
    ConfirmExisting {
        /// Branch to create or check out once the conflict is resolved.
        branch: String,
        /// Base ref for a new branch, or None for an existing-branch checkout.
        base: Option<String>,
        /// The conflicting directory.
        path: String,
        /// Name the directory is addressed by when it is a registered worktree.
        existing_name: Option<String>,
        /// 0 = Open (worktrees only), 1 = Replace, 2 = Cancel.
        selected: usize,
    },
    /// Replacing the existing directory would discard real work (uncommitted
    /// changes, or commits on its branch not yet in the default branch). Confirm
    /// the force delete before recreating.
    ConfirmReplaceChanges {
        /// Branch to create or check out once the directory is replaced.
        branch: String,
        /// Base ref for a new branch, or None for an existing-branch checkout.
        base: Option<String>,
        /// The conflicting directory, force-deleted on confirm.
        path: String,
        /// 0 = Force delete (lose all changes), 1 = Cancel.
        selected: usize,
    },
    /// Progress of an in-flight create running on a background thread.
    Creating {
        branch: String,
        lines: Vec<String>,
        rx: Receiver<CreateMsg>,
        done: bool,
        /// Handle for sending input to / killing the running setup command.
        control: SetupControl,
        /// Pending line of user input for a prompting setup command.
        input: String,
        /// True after one Ctrl+C; the next one kills the setup.
        kill_armed: bool,
    },
    /// Delete confirmation; `dirty` is the number of uncommitted changes.
    ConfirmDelete {
        name: String,
        dirty: usize,
        /// Branch checked out there, when not detached.
        branch: Option<String>,
        /// Currently selected option: also delete the branch afterwards.
        delete_branch: bool,
    },
    /// The worktree being deleted has uncommitted changes: keep the work with a
    /// stash, discard it (force-remove), or cancel.
    ConfirmDeleteDirty {
        name: String,
        /// Branch checked out there, carried through to the branch-delete step.
        branch: Option<String>,
        /// Whether to also delete the branch after the folder is removed.
        delete_branch: bool,
        /// 0 = Stash, 1 = Discard, 2 = Cancel.
        selected: usize,
    },
    /// The folder is gone but its branch could not be safely deleted; offer to
    /// force. `reason` explains why git refused so the wording can match.
    ConfirmForceBranch {
        branch: String,
        reason: ForceBranchReason,
    },
    /// Prompt for a one-off command to run in a worktree's directory, shown by
    /// the `e` key when no `open_command` is configured.
    RunCommand {
        name: String,
        path: String,
        input: TextInput,
    },
    /// First-run setup wizard, shown until `.wtm.toml` exists.
    Setup(Box<SetupWizard>),
    /// Editor for the repo's `.wtm.toml` settings.
    Config(Box<ConfigEditor>),
    /// Commit flow: pick which changed files to include (all by default) and
    /// type a message. Focus toggles between the file list and the message.
    Commit {
        name: String,
        files: Vec<StatusEntry>,
        /// Whether each file is staged for this commit, parallel with `files`.
        marked: Vec<bool>,
        /// Cursor into `files` while the file list has focus.
        cursor: usize,
        input: String,
        focus: CommitFocus,
    },
    /// Stash manager for one worktree.
    Stash {
        name: String,
        entries: Vec<StashEntry>,
        selected: usize,
        mode: StashMode,
    },
    /// Picker for switching the selected worktree onto a different existing
    /// local branch (one that isn't checked out anywhere else).
    Switch {
        /// Worktree being switched.
        name: String,
        /// Branches available to switch to (not checked out in any worktree).
        branches: Vec<String>,
        /// Live type-to-filter text; narrows `branches` by case-insensitive
        /// substring match.
        filter: TextInput,
        /// Cursor into the FILTERED branch list, not `branches` directly.
        selected: usize,
    },
    /// Scrollable commit log for one worktree.
    Log {
        name: String,
        entries: Vec<LogEntry>,
        scroll: u16,
    },
    /// Commit history of a branch on the Branches tab, with multi-select for
    /// cherry-picking. `marked` is parallel with `entries`; Enter opens the
    /// worktree picker (`CherryPick`) for the marked commits (or the one under
    /// the cursor when none are marked).
    BranchCommits {
        branch: String,
        entries: Vec<LogEntry>,
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
    /// A git operation (pull/push/fetch/delete/…) running on a background
    /// thread. Its result message is shown and the list refreshed when it
    /// finishes; `then` decides which view to reopen afterwards.
    Busy {
        label: String,
        rx: Receiver<Result<String, String>>,
        then: BusyThen,
    },
}

/// Which view to reopen once a `View::Busy` operation completes. Most ops land
/// back on the worktree list, but stash/branch ops return to their manager so
/// the user can keep working there.
pub enum BusyThen {
    List,
    Stash(String),
    Branch,
    /// After a backgrounded worktree removal succeeds, delete its branch on the
    /// main thread (so a refused delete can open the force prompt). Carries the
    /// worktree name and the branch to delete.
    DeleteBranch { name: String, branch: String },
}

/// A worktree the picked commits can be cherry-picked into. Cherry-pick needs a
/// working directory, so targets are always existing worktrees.
pub struct CherryTarget {
    /// Worktree name (how it's addressed in ops).
    pub name: String,
    /// Branch checked out there, or None when detached.
    pub branch: Option<String>,
}

/// Choice shown when adding the highlighted file or folder to `.gitignore`:
/// the exact path, or a glob pattern that ignores everything like it.
pub struct IgnorePrompt {
    /// Exact path of the file or folder, relative to the worktree root.
    /// Folder paths keep their trailing slash (e.g. `src/tui/`).
    pub file: String,
    /// Glob derived from the path (e.g. `*.log`), or the bare name.
    pub pattern: String,
    /// 0 = ignore just `file`; 1 = ignore `pattern`.
    pub selected: usize,
    /// True when the prompt targets a folder, which changes the wording.
    pub is_folder: bool,
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
/// before the first file it contains.
pub fn build_diff_rows(files: &[StatusEntry]) -> Vec<DiffRow> {
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
        for d in &dirs[common..] {
            stack.push((*d).to_string());
            rows.push(DiffRow::Folder {
                prefix: format!("{}/", stack.join("/")),
                label: (*d).to_string(),
                depth: stack.len() - 1,
            });
        }
        rows.push(DiffRow::File {
            index: idx,
            label: parts[parts.len() - 1].to_string(),
            depth: dirs.len(),
        });
    }
    rows
}

/// The `files` index for the row at `cursor`, or `None` when it is a folder.
pub fn current_file_index(rows: &[DiffRow], cursor: usize) -> Option<usize> {
    match rows.get(cursor) {
        Some(DiffRow::File { index, .. }) => Some(*index),
        _ => None,
    }
}

/// Branches from `branches` whose name contains `filter`, matched case
/// insensitively. An empty filter matches everything. Used by both the
/// switch-picker key handling and its renderer, so the two stay in sync.
pub fn filtered_branches<'a>(branches: &'a [String], filter: &str) -> Vec<&'a str> {
    let filter = filter.to_lowercase();
    branches
        .iter()
        .map(|b| b.as_str())
        .filter(|b| filter.is_empty() || b.to_lowercase().contains(&filter))
        .collect()
}

/// Which part of the commit dialog has keyboard focus.
#[derive(PartialEq, Eq)]
pub enum CommitFocus {
    /// The changed-file list: ↑/↓ move, Space toggles, `a` toggles all.
    Files,
    /// The commit message input: typing edits the message.
    Message,
}

/// Sub-state of the stash overlay.
pub enum StashMode {
    List,
    /// Typing an optional message for a new stash.
    Message(String),
    /// Confirming a drop of the selected entry.
    ConfirmDrop,
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

/// Sub-state of the branch tab.
pub enum BranchMode {
    List,
    /// Typing a name for a new branch.
    Create(String),
    /// Confirming deletion of the selected branch (`f` forces on refusal).
    ConfirmDelete,
}

/// The two top-level tabs of the main window. `View::List` renders whichever
/// tab is active; overlays (create, diff, switch, …) draw on top of it and
/// leave the active tab intact when they close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Worktrees,
    Branches,
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

impl RowList {
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

pub struct App {
    pub ctx: Ctx,
    pub worktrees: Vec<WorktreeInfo>,
    pub selected: usize,
    /// Active top-level tab. Only meaningful while `view` is `View::List`.
    pub tab: Tab,
    /// Branches shown on the Branches tab, loaded by `load_branches`.
    pub branches: Vec<BranchListItem>,
    /// Cursor into `branches` on the Branches tab.
    pub branch_selected: usize,
    /// Inline sub-state of the Branches tab (list, create-input, confirm-delete).
    pub branch_mode: BranchMode,
    pub view: View,
    /// Set by the renderer each frame; read by `on_mouse` to resolve clicks.
    pub row_list: Option<RowList>,
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
    /// Where new worktrees will be created, shown in the create dialog.
    pub worktree_base: Option<String>,
    /// Advances once per event-loop tick; drives the busy-overlay spinner.
    pub tick_count: u64,
    /// When true, the help overlay is drawn on top of the active view; the next
    /// key press dismisses it and returns to that view.
    pub show_help: bool,
    pub quit: bool,
}

impl App {
    pub fn new(ctx: Ctx) -> anyhow::Result<App> {
        let worktree_base = ctx
            .config
            .worktree_base(&ctx.repo_root)
            .ok()
            .map(|p| p.display().to_string());
        // An uninitialized repo opens into the setup wizard instead of the
        // worktree list; everything else waits until `.wtm.toml` exists.
        let initialized = setup::is_initialized(&ctx.repo_root);
        let view = if initialized {
            View::List
        } else {
            View::Setup(Box::new(SetupWizard::new(ctx.repo_root.clone())))
        };
        let mut app = App {
            ctx,
            worktrees: Vec::new(),
            selected: 0,
            tab: Tab::Worktrees,
            branches: Vec::new(),
            branch_selected: 0,
            branch_mode: BranchMode::List,
            view,
            row_list: None,
            message: None,
            message_at: None,
            message_shown: None,
            error: None,
            worktree_base,
            tick_count: 0,
            show_help: false,
            quit: false,
        };
        if initialized {
            app.refresh();
        }
        Ok(app)
    }

    /// Reloads the worktree list, keeping the selection in bounds.
    pub fn refresh(&mut self) {
        match ops::list(&self.ctx) {
            Ok(wts) => {
                self.worktrees = wts;
                self.selected = self.selected.min(self.worktrees.len().saturating_sub(1));
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Shows `msg` as a modal error popup (see `App::error`).
    fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
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
        if let View::Busy { rx, .. } = &self.view {
            if let Ok(result) = rx.try_recv() {
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
                    (Ok(_), BusyThen::DeleteBranch { name, branch }) => {
                        self.refresh();
                        self.delete_branch_step(name, branch);
                    }
                    (Ok(m), then) => {
                        self.message = Some(m);
                        self.refresh();
                        match then {
                            BusyThen::List | BusyThen::DeleteBranch { .. } => {}
                            BusyThen::Stash(name) => self.load_stash(name, StashMode::List),
                            BusyThen::Branch => {
                                self.branch_mode = BranchMode::List;
                                self.load_branches(self.branch_selected);
                            }
                        }
                    }
                    (Err(e), _) => {
                        self.set_error(e);
                        self.refresh();
                    }
                }
            }
            return;
        }
        if let View::Diff { last_refresh, .. } = &self.view {
            if last_refresh.elapsed() >= DIFF_REFRESH_INTERVAL {
                self.refresh_diff();
            }
            return;
        }
        let View::Creating {
            lines, rx, done, ..
        } = &mut self.view
        else {
            return;
        };
        if *done {
            return;
        }
        while let Ok(msg) = rx.try_recv() {
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
                    lines.push(if result.setup_ok {
                        format!("worktree ready: {}", result.path)
                    } else {
                        format!(
                            "worktree kept at {} but some setup steps failed",
                            result.path
                        )
                    });
                    lines.push("press Enter to continue".to_string());
                    *done = true;
                }
                CreateMsg::Done(Err(e)) => {
                    lines.push(format!("error: {e}"));
                    lines.push("press Enter to continue".to_string());
                    *done = true;
                }
            }
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

    pub fn on_key(&mut self, key: KeyEvent) {
        self.message = None;
        // A modal error popup swallows the very next key press, dismissing
        // itself rather than reaching Ctrl+C handling or the view underneath.
        if self.error.is_some() {
            self.error = None;
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
        // The help overlay swallows the next key press, returning to the view
        // underneath it.
        if self.show_help {
            self.show_help = false;
            return;
        }
        match &mut self.view {
            View::List => self.on_list_key(key),
            View::Diff { .. } => self.on_diff_key(key),
            View::Create { .. } => self.on_create_key(key),
            View::ConfirmExisting { .. } => self.on_confirm_existing_key(key),
            View::ConfirmReplaceChanges { selected, .. } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected < 1 {
                        *selected += 1;
                    }
                }
                KeyCode::Enter => self.apply_confirm_replace_changes(),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => self.view = View::List,
                _ => {}
            },
            View::RunCommand { input, .. } => match key.code {
                KeyCode::Esc => self.view = View::List,
                KeyCode::Enter => {
                    if let View::RunCommand { name, path, input } =
                        std::mem::replace(&mut self.view, View::List)
                    {
                        let cmd = input.trimmed();
                        if !cmd.is_empty() {
                            self.spawn_in_dir(&cmd, &path, &name);
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
                        self.view = View::List;
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
            View::ConfirmDelete { branch, delete_branch, .. } => match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    // Detached worktrees have no branch to offer deleting.
                    if branch.is_some() {
                        *delete_branch = !*delete_branch;
                    }
                }
                KeyCode::Enter | KeyCode::Char('y') => self.begin_delete(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.view = View::List,
                _ => {}
            },
            View::ConfirmDeleteDirty { selected, .. } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected < 2 {
                        *selected += 1;
                    }
                }
                KeyCode::Enter => self.apply_delete_dirty(),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => self.view = View::List,
                _ => {}
            },
            View::ConfirmForceBranch { branch, .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('f') | KeyCode::Char('y') => {
                    let branch = branch.clone();
                    match ops::force_delete_branch(&self.ctx, &branch) {
                        Ok(()) => {
                            self.message = Some(format!("deleted branch '{branch}' (forced)"));
                        }
                        Err(e) => self.set_error(format!("{e:#}")),
                    }
                    self.view = View::List;
                    self.refresh();
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.message = Some(format!("kept branch '{branch}'"));
                    self.view = View::List;
                    self.refresh();
                }
                _ => {}
            },
            View::Setup(wizard) => match wizard.on_key(key, &mut self.message) {
                WizardOutcome::Quit => self.quit = true,
                WizardOutcome::Done => {
                    let draft = wizard.draft.clone();
                    self.finish_setup(&draft);
                }
                WizardOutcome::Continue => {}
            },
            View::Config(editor) => match editor.on_key(key, &mut self.message) {
                EditorOutcome::Saved(path) => {
                    self.reload_config();
                    self.view = View::List;
                    if self.message.is_none() {
                        self.message = Some(format!("saved {}", path.display()));
                    }
                }
                EditorOutcome::Cancel => self.view = View::List,
                EditorOutcome::Continue => {}
            },
            View::Commit { .. } => self.on_commit_key(key),
            View::Stash { .. } => self.on_stash_key(key),
            View::Switch { .. } => self.on_switch_key(key),
            View::Log { .. } => self.on_log_key(key),
            View::BranchCommits { .. } => self.on_branch_commits_key(key),
            View::CherryPick { .. } => self.on_cherry_pick_key(key),
            // A background op owns the screen until tick() drains its result.
            View::Busy { .. } => {}
        }
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
                self.view = View::List;
                self.refresh();
                self.message = Some(format!("wrote {}", crate::config::CONFIG_FILE));
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Home-view key handling: cycle tabs, then dispatch to the active tab.
    fn on_list_key(&mut self, key: KeyEvent) {
        // Tab / Shift+Tab cycle the top-level tabs, except while the Branches
        // tab is capturing text for a new branch name.
        let typing_branch =
            self.tab == Tab::Branches && matches!(self.branch_mode, BranchMode::Create(_));
        if !typing_branch && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_tab();
            return;
        }
        match self.tab {
            Tab::Worktrees => self.on_worktrees_tab_key(key),
            Tab::Branches => self.on_branches_tab_key(key),
        }
    }

    /// Switches to the other top-level tab.
    fn toggle_tab(&mut self) {
        match self.tab {
            Tab::Worktrees => self.open_branches_tab(),
            Tab::Branches => self.tab = Tab::Worktrees,
        }
    }

    fn on_worktrees_tab_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.worktrees.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('r') => {
                self.refresh();
                self.message = Some("refreshed".to_string());
            }
            KeyCode::Char('n') => self.open_create(),
            KeyCode::Char('c') => self.open_commit(),
            KeyCode::Char('o') => match ConfigEditor::load(self.ctx.repo_root.clone()) {
                Ok(editor) => self.view = View::Config(Box::new(editor)),
                Err(e) => self.set_error(format!("{e:#}")),
            },
            KeyCode::Char('e') => self.run_open_command(),
            KeyCode::Char('s') => self.open_stash(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.start_push(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('b') => self.open_switch(),
            KeyCode::Char('l') => self.open_log(),
            KeyCode::Char('d') => {
                if let Some(wt) = self.selected_worktree() {
                    if wt.is_main {
                        self.message = Some("cannot remove the main worktree".to_string());
                    } else {
                        self.view = View::ConfirmDelete {
                            name: wt.name.clone(),
                            dirty: wt.dirty,
                            branch: wt.branch.clone(),
                            delete_branch: false,
                        };
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(wt) = self.selected_worktree() {
                    let name = wt.name.clone();
                    self.open_diff(name);
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    /// Opens the per-file changes view for the worktree named `name`.
    fn open_diff(&mut self, name: String) {
        match ops::status(&self.ctx, &name) {
            Ok((_, files)) => {
                let marked = vec![true; files.len()];
                let rows = build_diff_rows(&files);
                self.view = View::Diff {
                    name,
                    files,
                    marked,
                    rows,
                    selected: 0,
                    content: String::new(),
                    scroll: 0,
                    last_refresh: Instant::now(),
                    confirm_revert: false,
                    ignore_prompt: None,
                };
                self.load_diff_content(true);
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Loads the diff text for the file under the cursor into the Diff view.
    /// When the cursor sits on a folder row there is no diff to show, so the
    /// content is cleared. `reset_scroll` sends the viewport back to the top
    /// (used when the selected file changes); otherwise the current scroll is
    /// kept and merely clamped to the new content, so the periodic auto-refresh
    /// doesn't yank the user back to the top of the file they're reading.
    fn load_diff_content(&mut self, reset_scroll: bool) {
        let View::Diff {
            name,
            files,
            rows,
            selected,
            ..
        } = &self.view
        else {
            return;
        };
        let entry = current_file_index(rows, *selected).and_then(|i| files.get(i).cloned());
        let name = name.clone();
        let content = match entry {
            Some(e) => {
                let untracked = e.code.starts_with('?');
                match ops::file_diff(&self.ctx, &name, &e.path, untracked) {
                    Ok(c) => c,
                    Err(err) => format!("error: {err:#}"),
                }
            }
            None => String::new(),
        };
        if let View::Diff {
            content: slot,
            scroll,
            ..
        } = &mut self.view
        {
            *slot = content;
            if reset_scroll {
                *scroll = 0;
            } else {
                // Keep the reader's place, but don't let a shrunken diff leave
                // the viewport scrolled past the last line.
                let max = slot.lines().count().saturating_sub(1) as u16;
                *scroll = (*scroll).min(max);
            }
        }
    }

    /// Handles mouse input. The scroll wheel moves the diff or log viewport;
    /// other mouse events are ignored.
    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        // A left click moves the selection to the clicked row, mirroring the
        // arrow keys.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.on_click(mouse.column, mouse.row);
            return;
        }
        // Scroll three lines per wheel notch, matching Shift+Up/Down.
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => |s: u16| s.saturating_add(3),
            MouseEventKind::ScrollUp => |s: u16| s.saturating_sub(3),
            _ => return,
        };
        match &mut self.view {
            View::Diff { scroll, .. } | View::Log { scroll, .. } => *scroll = delta(*scroll),
            _ => {}
        }
    }

    /// Selects the list row under a left click, if one landed on the active
    /// view's clickable list. Loads the diff for a newly selected file so a
    /// click behaves exactly like arrowing onto the row.
    fn on_click(&mut self, col: u16, row: u16) {
        let Some(idx) = self.row_list.and_then(|rl| rl.hit(col, row)) else {
            return;
        };
        match self.view {
            View::List => match self.tab {
                Tab::Worktrees => {
                    if idx < self.worktrees.len() {
                        self.selected = idx;
                    }
                }
                Tab::Branches => {
                    if idx < self.branches.len() {
                        self.branch_selected = idx;
                    }
                }
            },
            View::Diff { .. } => {
                if let View::Diff { selected, rows, .. } = &mut self.view {
                    if idx >= rows.len() || *selected == idx {
                        return;
                    }
                    *selected = idx;
                }
                self.load_diff_content(true);
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
            _ => {}
        }
    }

    fn on_diff_key(&mut self, key: KeyEvent) {
        let View::Diff {
            files,
            marked,
            rows,
            selected,
            confirm_revert,
            ignore_prompt,
            ..
        } = &mut self.view
        else {
            return;
        };
        if *confirm_revert {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let entry =
                        current_file_index(rows, *selected).and_then(|i| files.get(i).cloned());
                    *confirm_revert = false;
                    if let Some(e) = entry {
                        self.revert_file(e);
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => *confirm_revert = false,
                _ => {}
            }
            return;
        }
        if ignore_prompt.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                    if let Some(p) = ignore_prompt {
                        p.selected ^= 1;
                    }
                }
                KeyCode::Enter => {
                    let pattern = ignore_prompt
                        .take()
                        .map(|p| if p.selected == 0 { p.file } else { p.pattern });
                    if let Some(pattern) = pattern {
                        self.add_ignore(&pattern);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => *ignore_prompt = None,
                _ => {}
            }
            return;
        }
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
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
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
            KeyCode::Home | KeyCode::Char('g') => self.scroll_diff(|_| 0),
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
            KeyCode::Char('R') => {
                if current_file_index(rows, *selected).is_some() {
                    *confirm_revert = true;
                }
            }
            KeyCode::Char('i') => match rows.get(*selected) {
                Some(DiffRow::File { index, .. }) => {
                    if let Some(entry) = files.get(*index) {
                        *ignore_prompt = Some(IgnorePrompt {
                            file: entry.path.clone(),
                            pattern: ops::ignore_pattern(&entry.path),
                            selected: 0,
                            is_folder: false,
                        });
                    }
                }
                Some(DiffRow::Folder { prefix, label, .. }) => {
                    *ignore_prompt = Some(IgnorePrompt {
                        file: prefix.clone(),
                        pattern: format!("{label}/"),
                        selected: 0,
                        is_folder: true,
                    });
                }
                None => {}
            },
            KeyCode::Char('c') => self.commit_from_diff(),
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    /// Adds `pattern` to the worktree's `.gitignore`, then reloads the view.
    fn add_ignore(&mut self, pattern: &str) {
        let View::Diff { name, .. } = &self.view else {
            return;
        };
        let name = name.clone();
        match ops::add_to_gitignore(&self.ctx, &name, pattern) {
            Ok(true) => self.message = Some(format!("added '{pattern}' to .gitignore")),
            Ok(false) => self.message = Some(format!("'{pattern}' is already in .gitignore")),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Applies `f` to the diff scroll offset, if the diff view is active.
    fn scroll_diff(&mut self, f: impl FnOnce(u16) -> u16) {
        if let View::Diff { scroll, .. } = &mut self.view {
            *scroll = f(*scroll);
        }
    }

    /// Rebuilds the changed-file list and the selected file's diff in place,
    /// preserving commit marks by path and clamping the cursor. No-op outside
    /// the diff view.
    fn refresh_diff(&mut self) {
        let View::Diff { name, .. } = &self.view else {
            return;
        };
        let name = name.clone();
        // Remember which file is under the cursor so we can tell whether the
        // refresh lands on the same file (keep scroll) or a different one
        // because the list shifted (reset scroll).
        let old_path = if let View::Diff {
            files,
            rows,
            selected,
            ..
        } = &self.view
        {
            current_file_index(rows, *selected)
                .and_then(|i| files.get(i))
                .map(|f| f.path.clone())
        } else {
            None
        };
        match ops::status(&self.ctx, &name) {
            Ok((_, new_files)) => {
                if let View::Diff {
                    files,
                    marked,
                    rows,
                    selected,
                    last_refresh,
                    ..
                } = &mut self.view
                {
                    // Carry commit marks over to files that still exist.
                    let old: std::collections::HashMap<&str, bool> = files
                        .iter()
                        .zip(marked.iter())
                        .map(|(f, m)| (f.path.as_str(), *m))
                        .collect();
                    let new_marked = new_files
                        .iter()
                        .map(|f| old.get(f.path.as_str()).copied().unwrap_or(true))
                        .collect();
                    *rows = build_diff_rows(&new_files);
                    *files = new_files;
                    *marked = new_marked;
                    *selected = (*selected).min(rows.len().saturating_sub(1));
                    *last_refresh = Instant::now();
                }
                let new_path = if let View::Diff {
                    files,
                    rows,
                    selected,
                    ..
                } = &self.view
                {
                    current_file_index(rows, *selected)
                        .and_then(|i| files.get(i))
                        .map(|f| f.path.clone())
                } else {
                    None
                };
                self.load_diff_content(new_path != old_path);
            }
            // The worktree may have been removed out from under us; surface it
            // and drop back to the list rather than looping on the error.
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.view = View::List;
                self.refresh();
            }
        }
    }

    /// Stashes a single file from the diff view, then reloads it.
    fn stash_file(&mut self, entry: StatusEntry) {
        let View::Diff { name, .. } = &self.view else {
            return;
        };
        let name = name.clone();
        match ops::stash_push_paths(&self.ctx, &name, std::slice::from_ref(&entry.path), None) {
            Ok(_) => self.message = Some(format!("stashed '{}'", entry.path)),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Stashes every marked (`[x]`) file from the diff view, then reloads it.
    /// Reports when nothing is marked rather than stashing the whole worktree.
    fn stash_marked(&mut self) {
        let View::Diff {
            name,
            files,
            marked,
            ..
        } = &self.view
        else {
            return;
        };
        let name = name.clone();
        let paths: Vec<String> = files
            .iter()
            .zip(marked.iter())
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

    /// Reverts a single file from the diff view, then reloads it.
    fn revert_file(&mut self, entry: StatusEntry) {
        let View::Diff { name, .. } = &self.view else {
            return;
        };
        let name = name.clone();
        let untracked = entry.code.starts_with('?');
        match ops::revert_file(&self.ctx, &name, &entry.path, untracked) {
            Ok(_) => self.message = Some(format!("reverted '{}'", entry.path)),
            Err(e) => self.set_error(format!("{e:#}")),
        }
        self.refresh_diff();
        self.refresh();
    }

    /// Opens the commit dialog from the diff view, carrying the files marked
    /// there as the initial selection.
    fn commit_from_diff(&mut self) {
        let View::Diff {
            name,
            files,
            marked,
            ..
        } = &self.view
        else {
            return;
        };
        if files.is_empty() {
            self.message = Some("nothing to commit".to_string());
            return;
        }
        self.view = View::Commit {
            name: name.clone(),
            files: files.clone(),
            marked: marked.clone(),
            cursor: 0,
            input: String::new(),
            focus: CommitFocus::Message,
        };
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
        let branches: Vec<String> = all_branches
            .iter()
            .filter(|b| !checked_out.contains(&b.as_str()))
            .cloned()
            .collect();
        let base = self.default_base(&all_branches);
        self.view = View::Create {
            name: TextInput::default(),
            branches,
            all_branches,
            base,
            selected: 0,
            base_focus: false,
            base_pick: None,
        };
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
        let open_base_pick = |base: &str, all_branches: &[String], base_pick: &mut Option<usize>| {
            let start = all_branches.iter().position(|b| b == base).unwrap_or(0);
            *base_pick = Some(start);
        };
        match key.code {
            // Esc backs out of the focused base button first, then the dialog.
            KeyCode::Esc => {
                if *base_focus {
                    *base_focus = false;
                } else {
                    self.view = View::List;
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
                if *selected < branches.len() {
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
                    let branch = branches[*selected - 1].clone();
                    self.request_create(branch, None);
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

    /// Starts a create for `branch` (new branch when `base` is `Some`), first
    /// checking whether the target directory already exists and, if so, asking
    /// the user what to do about it.
    fn request_create(&mut self, branch: String, base: Option<String>) {
        match ops::existing_target(&self.ctx, &branch) {
            Ok(Some(target)) => {
                self.view = View::ConfirmExisting {
                    branch,
                    base,
                    path: target.path.to_string_lossy().to_string(),
                    existing_name: target.worktree_name,
                    // Default to Open when it's a worktree, else Replace.
                    selected: 0,
                };
            }
            Ok(None) => self.start_create(branch, base),
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Drives the "directory already exists" prompt: Open an existing worktree,
    /// Replace the directory, or Cancel.
    fn on_confirm_existing_key(&mut self, key: KeyEvent) {
        let View::ConfirmExisting {
            existing_name,
            selected,
            ..
        } = &mut self.view
        else {
            return;
        };
        // Without a worktree to open, only Replace (1) and Cancel (2) apply.
        let first = if existing_name.is_some() { 0 } else { 1 };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = (*selected).saturating_sub(1).max(first);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected < 2 {
                    *selected += 1;
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
            KeyCode::Enter => {
                if *selected < first {
                    *selected = first;
                }
                self.apply_confirm_existing();
            }
            _ => {}
        }
    }

    /// Carries out the choice made in the "directory already exists" prompt.
    fn apply_confirm_existing(&mut self) {
        let View::ConfirmExisting {
            branch,
            base,
            path,
            existing_name,
            selected,
        } = std::mem::replace(&mut self.view, View::List)
        else {
            return;
        };
        match selected {
            // Open the existing worktree.
            0 => match existing_name {
                Some(name) => self.open_diff(name),
                None => self.message = Some("that directory is not a worktree".to_string()),
            },
            // Replace: remove the directory, then create fresh. Only stop to
            // confirm when the occupying worktree holds work that would be lost.
            1 => match ops::target_has_changes(&self.ctx, Path::new(&path)) {
                Ok(true) => {
                    self.view = View::ConfirmReplaceChanges {
                        branch,
                        base,
                        path,
                        selected: 1,
                    };
                }
                Ok(false) => self.replace_target(branch, base, &path),
                Err(e) => self.set_error(format!("{e:#}")),
            },
            // Cancel.
            _ => {}
        }
    }

    /// Carries out the force-delete confirmation shown when replacing a
    /// directory that holds real work: Force delete removes it and recreates,
    /// Cancel returns to the list.
    fn apply_confirm_replace_changes(&mut self) {
        let View::ConfirmReplaceChanges {
            branch,
            base,
            path,
            selected,
        } = std::mem::replace(&mut self.view, View::List)
        else {
            return;
        };
        // 0 = Force delete; anything else cancels back to the list.
        if selected == 0 {
            self.replace_target(branch, base, &path);
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
            rx,
            done: false,
            control,
            input: String::new(),
            kill_armed: false,
        };
    }

    /// Runs the configured `open_command` in the selected worktree's directory,
    /// or opens a prompt for a one-off command when none is configured.
    fn run_open_command(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let path = wt.path.clone();
        let name = wt.name.clone();
        match self.ctx.config.open_command.clone() {
            Some(cmd) if !cmd.trim().is_empty() => self.spawn_in_dir(cmd.trim(), &path, &name),
            _ => {
                self.view = View::RunCommand {
                    name,
                    path,
                    input: TextInput::default(),
                }
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
                self.view = View::Commit {
                    name,
                    files,
                    marked,
                    cursor: 0,
                    input: String::new(),
                    focus: CommitFocus::Message,
                };
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
            focus,
            ..
        } = &mut self.view
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.view = View::List;
                return;
            }
            KeyCode::Tab => {
                *focus = match focus {
                    CommitFocus::Files => CommitFocus::Message,
                    CommitFocus::Message => CommitFocus::Files,
                };
                return;
            }
            KeyCode::Enter => {
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
            CommitFocus::Message => match key.code {
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
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
            ..
        } = &self.view
        else {
            return;
        };
        let message = input.trim().to_string();
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
                ops::commit(ctx, &name, &message, Some(&paths))
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

    /// Opens the stash manager for the selected worktree.
    fn open_stash(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        self.load_stash(name, StashMode::List);
    }

    /// (Re)loads the stash list for `name` and shows the overlay in `mode`.
    /// Falls back to the list view when the stashes can't be read.
    fn load_stash(&mut self, name: String, mode: StashMode) {
        match ops::stash_list(&self.ctx, &name) {
            Ok(r) => {
                self.view = View::Stash {
                    name,
                    entries: r.entries,
                    selected: 0,
                    mode,
                };
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.view = View::List;
            }
        }
    }

    fn on_stash_key(&mut self, key: KeyEvent) {
        let View::Stash {
            name,
            entries,
            selected,
            mode,
        } = &mut self.view
        else {
            return;
        };
        match mode {
            StashMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Char('s') => *mode = StashMode::Message(String::new()),
                KeyCode::Char('p') => {
                    let name = name.clone();
                    let index = entries.get(*selected).map(|e| e.index);
                    self.stash_action("pop", name, index);
                }
                KeyCode::Char('a') => {
                    let name = name.clone();
                    let index = entries.get(*selected).map(|e| e.index);
                    self.stash_action("apply", name, index);
                }
                KeyCode::Char('x') => {
                    if !entries.is_empty() {
                        *mode = StashMode::ConfirmDrop;
                    }
                }
                _ => {}
            },
            StashMode::Message(buf) => match key.code {
                KeyCode::Esc => *mode = StashMode::List,
                KeyCode::Enter => {
                    let name = name.clone();
                    let msg = buf.trim().to_string();
                    let msg = if msg.is_empty() { None } else { Some(msg) };
                    self.stash_push(name, msg);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            StashMode::ConfirmDrop => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let name = name.clone();
                    let index = entries.get(*selected).map(|e| e.index);
                    self.stash_action("drop", name, index);
                }
                KeyCode::Esc | KeyCode::Char('n') => *mode = StashMode::List,
                _ => {}
            },
        }
    }

    /// Runs a pop/apply/drop on `name`, reports the result, and reloads the
    /// overlay (dirty counts and the stash list may both have changed).
    fn stash_action(&mut self, action: &str, name: String, index: Option<u32>) {
        let action = action.to_string();
        self.start_busy(
            format!("stash {action}…"),
            BusyThen::Stash(name.clone()),
            move |ctx| {
                let result = match action.as_str() {
                    "pop" => ops::stash_pop(ctx, &name, index),
                    "apply" => ops::stash_apply(ctx, &name, index),
                    _ => ops::stash_drop(ctx, &name, index),
                };
                result
                    .map(|r| format!("stash {} on '{}'", r.action, r.name))
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
    /// not checked out in any worktree (so git will let us switch onto them).
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
        let branches: Vec<String> = match crate::git::local_branches(&self.ctx.repo_root) {
            Ok(all) => all
                .into_iter()
                .filter(|b| !checked_out.contains(b))
                .collect(),
            Err(e) => {
                self.set_error(format!("{e:#}"));
                return;
            }
        };
        if branches.is_empty() {
            self.message = Some("no other branches available to switch to".to_string());
            return;
        }
        self.view = View::Switch {
            name,
            branches,
            filter: TextInput::default(),
            selected: 0,
        };
    }

    /// Drives the switch-branch picker: type to filter the branch list, move
    /// the cursor within the filtered results, or switch on Enter. Esc clears
    /// an active filter first, then closes the view on a second press.
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
                    self.view = View::List;
                }
            }
            KeyCode::Down => {
                let count = filtered_branches(branches, filter.as_str()).len();
                if *selected + 1 < count {
                    *selected += 1;
                }
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Enter => {
                let branch = filtered_branches(branches, filter.as_str())
                    .get(*selected)
                    .map(|b| (*b).to_string());
                if let Some(branch) = branch {
                    let name = name.clone();
                    self.request_switch(name, branch);
                }
            }
            _ => {
                if filter.on_key(key) {
                    // The filtered set just changed; keep the cursor in bounds
                    // rather than pointing past the new (likely shorter) list.
                    let count = filtered_branches(branches, filter.as_str()).len();
                    *selected = (*selected).min(count.saturating_sub(1));
                }
            }
        }
    }

    /// Switches the worktree named `name` onto `branch` in the background.
    fn request_switch(&mut self, name: String, branch: String) {
        self.start_busy(
            format!("switching {name} to {branch}…"),
            BusyThen::List,
            move |ctx| {
                ops::switch_branch(ctx, &name, &branch)
                    .map(|r| format!("switched '{}' to '{}'", r.name, r.branch))
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Switches to the Branches tab, loading the branch list fresh.
    fn open_branches_tab(&mut self) {
        self.tab = Tab::Branches;
        self.branch_mode = BranchMode::List;
        self.load_branches(0);
    }

    /// (Re)loads all local branches for the Branches tab, clamping the cursor.
    /// Bounces back to the Worktrees tab on error.
    fn load_branches(&mut self, selected: usize) {
        match ops::branch_list(&self.ctx) {
            Ok(r) => {
                self.branch_selected = selected.min(r.branches.len().saturating_sub(1));
                self.branches = r.branches;
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.tab = Tab::Worktrees;
            }
        }
    }

    /// Key handling for the Branches tab (active when `view` is `List` and
    /// `tab` is `Branches`).
    fn on_branches_tab_key(&mut self, key: KeyEvent) {
        // Text-entry mode owns keystrokes while naming a new branch.
        if let BranchMode::Create(buf) = &mut self.branch_mode {
            match key.code {
                KeyCode::Esc => self.branch_mode = BranchMode::List,
                KeyCode::Enter => {
                    let name = buf.trim().to_string();
                    if name.is_empty() {
                        self.message = Some("branch name must not be empty".to_string());
                        return;
                    }
                    self.branch_create(name);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return;
        }
        if matches!(self.branch_mode, BranchMode::ConfirmDelete) {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    if let Some(name) = self
                        .branches
                        .get(self.branch_selected)
                        .map(|b| b.name.clone())
                    {
                        self.branch_delete(name, false);
                    }
                }
                KeyCode::Char('f') => {
                    if let Some(name) = self
                        .branches
                        .get(self.branch_selected)
                        .map(|b| b.name.clone())
                    {
                        self.branch_delete(name, true);
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => self.branch_mode = BranchMode::List,
                _ => {}
            }
            return;
        }
        // BranchMode::List
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
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
            KeyCode::Char('n') => self.branch_mode = BranchMode::Create(String::new()),
            KeyCode::Char('d') => {
                if !self.branches.is_empty() {
                    self.branch_mode = BranchMode::ConfirmDelete;
                }
            }
            // Enter drills into the branch's commit history, the entry point
            // for cherry-picking commits into a worktree.
            KeyCode::Enter => self.open_branch_commits(),
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
            KeyCode::Char('?') => self.show_help = true,
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
        match ops::branch_log(&self.ctx, &branch, 200) {
            Ok(r) => {
                let marked = vec![false; r.entries.len()];
                self.view = View::BranchCommits {
                    branch,
                    entries: r.entries,
                    marked,
                    selected: 0,
                };
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    /// Key handling for the branch commit-history view: move the cursor, toggle
    /// commits for cherry-picking, and open the worktree picker.
    fn on_branch_commits_key(&mut self, key: KeyEvent) {
        let View::BranchCommits {
            entries,
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
                self.view = View::List;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < entries.len() {
                    *selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(m) = marked.get_mut(*selected) {
                    *m = !*m;
                }
            }
            KeyCode::Char('a') => {
                let all = marked.iter().all(|m| *m);
                for m in marked.iter_mut() {
                    *m = !all;
                }
            }
            KeyCode::Enter => self.open_cherry_pick(),
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
    }

    /// Builds the cherry-pick worktree picker from the marked commits (or the
    /// one under the cursor when none are marked). Commits are ordered
    /// oldest-first, the order git applies them.
    fn open_cherry_pick(&mut self) {
        let View::BranchCommits {
            branch,
            entries,
            marked,
            selected,
        } = &self.view
        else {
            return;
        };
        // Gather chosen commits newest-first as they appear, then reverse to
        // oldest-first for git.
        let chosen: Vec<usize> = if marked.iter().any(|m| *m) {
            (0..entries.len()).filter(|i| marked[*i]).collect()
        } else {
            vec![*selected]
        };
        if chosen.is_empty() {
            return;
        }
        let mut commits: Vec<String> = Vec::new();
        let mut summaries: Vec<String> = Vec::new();
        for &i in chosen.iter().rev() {
            if let Some(e) = entries.get(i) {
                commits.push(e.hash.clone());
                summaries.push(e.subject.clone());
            }
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
        self.view = View::CherryPick {
            source_branch,
            commits,
            summaries,
            targets,
            selected: 0,
            mode: None,
        };
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
                KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
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
        let verb = if no_commit { "loading" } else { "cherry-picking" };
        self.start_busy(
            format!("{verb} {count} commit(s) into '{target_name}'…"),
            BusyThen::List,
            move |ctx| {
                ops::cherry_pick(ctx, &target_name, &commits, no_commit)
                    .map(|r| {
                        if r.committed {
                            format!("cherry-picked {} commit(s) into '{}'", r.count, r.target)
                        } else {
                            format!(
                                "loaded {} commit(s) into '{}' (review, then commit)",
                                r.count, r.target
                            )
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
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

    /// Deletes a branch. A refused non-force delete keeps the confirm open so
    /// the user can retry with `f` (force). Runs synchronously (a fast local
    /// op) so that retry flow stays intact.
    fn branch_delete(&mut self, name: String, force: bool) {
        match ops::branch_delete(&self.ctx, &name, force) {
            Ok(r) => {
                self.message = Some(format!(
                    "deleted branch '{}'{}",
                    r.name,
                    if r.forced { " (forced)" } else { "" }
                ));
                self.branch_mode = BranchMode::List;
                self.load_branches(self.branch_selected);
            }
            Err(e) => self.set_error(format!("{e:#} — press f to force")),
        }
    }

    /// Opens the new-worktree dialog prefilled with `branch`, used when the
    /// Branches tab targets a branch that isn't checked out anywhere.
    fn open_create_prefilled(&mut self, branch: String) {
        self.open_create();
        // The branch browser picks an existing branch to check out, so select
        // it in the checkout list rather than the new-branch row.
        if let View::Create {
            branches, selected, ..
        } = &mut self.view
            && let Some(pos) = branches.iter().position(|b| *b == branch)
        {
            *selected = pos + 1;
        }
    }

    /// Opens the scrollable commit log for the selected worktree.
    fn open_log(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        match ops::log(&self.ctx, &name, 100) {
            Ok(r) => {
                self.view = View::Log {
                    name,
                    entries: r.entries,
                    scroll: 0,
                }
            }
            Err(e) => self.set_error(format!("{e:#}")),
        }
    }

    fn on_log_key(&mut self, key: KeyEvent) {
        let View::Log { scroll, .. } = &mut self.view else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
            KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::PageDown => *scroll = scroll.saturating_add(20),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
            KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
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
        self.view = View::Busy { label, rx, then };
    }

    /// Pulls the selected worktree (fast-forward only) in the background.
    fn start_pull(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        self.start_busy(format!("pulling {name}…"), BusyThen::List, move |ctx| {
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

    /// Pushes the selected worktree (auto-publishing when it has no upstream).
    fn start_push(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
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

    /// Fetches all remotes (with prune) in the background.
    fn start_fetch(&mut self) {
        self.start_busy(
            "fetching all remotes…".to_string(),
            BusyThen::List,
            move |ctx| {
                ops::fetch(ctx)
                    .map(|r| {
                        if r.remotes.is_empty() {
                            "no remotes to fetch".to_string()
                        } else {
                            format!("fetched: {}", r.remotes.join(", "))
                        }
                    })
                    .map_err(|e| format!("{e:#}"))
            },
        );
    }

    /// Starts the delete flow from the `ConfirmDelete` prompt. A dirty worktree
    /// first routes through the Stash / Discard prompt; a clean one proceeds
    /// straight to removal.
    fn begin_delete(&mut self) {
        let View::ConfirmDelete {
            name,
            dirty,
            branch,
            delete_branch,
        } = &self.view
        else {
            return;
        };
        let (name, cached_dirty, branch, delete_branch) =
            (name.clone(), *dirty, branch.clone(), *delete_branch);
        // Re-check dirtiness live rather than trusting the count captured when
        // the list was loaded, since the worktree may have changed since then.
        let dirty = ops::worktree_is_dirty(&self.ctx, &name).unwrap_or(cached_dirty > 0);
        if dirty {
            self.view = View::ConfirmDeleteDirty {
                name,
                branch,
                delete_branch,
                selected: 0,
            };
        } else {
            self.do_delete(name, branch, delete_branch, false);
        }
    }

    /// Carries out the Stash / Discard / Cancel choice for a dirty worktree.
    fn apply_delete_dirty(&mut self) {
        let View::ConfirmDeleteDirty {
            name,
            branch,
            delete_branch,
            selected,
        } = &self.view
        else {
            return;
        };
        let (name, branch, delete_branch, selected) =
            (name.clone(), branch.clone(), *delete_branch, *selected);
        match selected {
            // Stash: keep the work, then remove the now-clean folder.
            0 => match ops::stash_worktree(&self.ctx, &name) {
                Ok(()) => self.do_delete(name, branch, delete_branch, false),
                Err(e) => {
                    self.set_error(format!("{e:#}"));
                    self.view = View::List;
                    self.refresh();
                }
            },
            // Discard: force-remove the folder, throwing the changes away.
            1 => self.do_delete(name, branch, delete_branch, true),
            // Cancel.
            _ => self.view = View::List,
        }
    }

    /// Removes the worktree folder and, when requested, deletes its branch. A
    /// folder-only removal is backgrounded through the Busy overlay; a branch
    /// delete runs synchronously so an unmerged or checked-out-elsewhere
    /// refusal can open the force prompt instead of failing silently.
    fn do_delete(&mut self, name: String, branch: Option<String>, delete_branch: bool, force: bool) {
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
                self.start_busy(
                    format!("removing '{name}'…"),
                    BusyThen::List,
                    move |ctx| {
                        ops::remove_worktree_only(ctx, &thread_name, force)
                            .map(|info| match &info.branch {
                                Some(_) => format!("removed '{}' (branch kept)", info.name),
                                None => format!("removed '{}'", info.name),
                            })
                            .map_err(|e| format!("{e:#}"))
                    },
                );
            }
        }
    }

    /// After the folder is removed, attempts a safe branch delete and routes to
    /// the matching force prompt when git refuses.
    fn delete_branch_step(&mut self, name: String, branch: String) {
        match ops::try_delete_branch(&self.ctx, &branch) {
            Ok(ops::DeleteBranchOutcome::Deleted) => {
                self.message = Some(format!("removed '{name}' and branch '{branch}'"));
                self.view = View::List;
                self.refresh();
            }
            Ok(ops::DeleteBranchOutcome::NotMerged) => {
                // Refresh so the now-removed folder drops from the list behind
                // the popup.
                self.refresh();
                self.view = View::ConfirmForceBranch {
                    branch,
                    reason: ForceBranchReason::NotMerged,
                };
            }
            Ok(ops::DeleteBranchOutcome::CheckedOutElsewhere(other)) => {
                self.refresh();
                self.view = View::ConfirmForceBranch {
                    branch,
                    reason: ForceBranchReason::CheckedOutElsewhere(other),
                };
            }
            Err(e) => {
                self.set_error(format!("{e:#}"));
                self.view = View::List;
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
        let app = App::new(ctx).unwrap();
        (tmp, app)
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

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
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
    }

    fn press_shift(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::SHIFT));
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
    }

    fn ctrl_c(app: &mut App) {
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    /// Moves the diff view's cursor onto the row for the file named `path`,
    /// panicking if it isn't in the list. Skips over folder rows.
    fn select_diff_file(app: &mut App, path: &str) {
        loop {
            match &app.view {
                View::Diff {
                    files,
                    rows,
                    selected,
                    ..
                } => {
                    if let Some(i) = current_file_index(rows, *selected)
                        && files[i].path == path
                    {
                        return;
                    }
                    assert!(*selected + 1 < rows.len(), "{path} not in the diff list");
                }
                _ => panic!("expected diff view"),
            }
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
        // Any key closes the overlay, returning to the underlying view.
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.show_help);
        assert!(matches!(app.view, View::List));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn any_key_dismisses_the_error_popup() {
        let (_tmp, mut app) = test_app();
        app.set_error("boom");
        assert!(app.error.is_some());
        // Any key closes the popup instead of reaching the view underneath.
        press(&mut app, KeyCode::Char('x'));
        assert!(app.error.is_none());
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
                assert!(branches.contains(&"spare".to_string()));
                assert!(branches.contains(&"other".to_string()));
                assert!(!branches.contains(&"main".to_string()));
            }
            _ => panic!("expected create dialog"),
        }
        // ↓ into the checkout list, then pick the highlighted existing branch.
        press(&mut app, KeyCode::Down);
        let expected = match &app.view {
            View::Create {
                branches, selected, ..
            } => branches[*selected - 1].clone(),
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
        assert_eq!(app.tab, Tab::Branches);
        // Entering the Branches tab loads the branch list.
        assert!(!app.branches.is_empty());
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Worktrees);
    }

    #[test]
    fn switch_with_no_other_branches_reports_message() {
        let (_tmp, mut app) = test_app();
        // Only the main branch exists and it is checked out, so there is nothing
        // to switch onto: the picker doesn't open and a message explains why.
        press(&mut app, KeyCode::Char('b'));
        assert!(matches!(app.view, View::List));
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("no other branches")
        );
    }

    #[test]
    fn main_worktree_cannot_be_deleted() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.view, View::List));
        assert!(app.message.as_deref().unwrap().contains("main worktree"));
    }

    #[test]
    fn enter_opens_diff_and_scrolls() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Diff { files, .. } => assert!(!files.is_empty(), "the untracked .wtm.toml shows"),
            _ => panic!("expected diff view"),
        }
        // Shift+Down scrolls the diff content; each press moves three lines.
        press_shift(&mut app, KeyCode::Down);
        press_shift(&mut app, KeyCode::Down);
        match &app.view {
            View::Diff { scroll, .. } => assert_eq!(*scroll, 6),
            _ => panic!("expected diff view"),
        }
        // Capital J/K scroll on terminals that don't report the Shift modifier
        // on arrow keys; the mouse wheel scrolls too.
        press(&mut app, KeyCode::Char('J'));
        match &app.view {
            View::Diff { scroll, .. } => assert_eq!(*scroll, 9),
            _ => panic!("expected diff view"),
        }
        scroll_wheel(&mut app, MouseEventKind::ScrollUp);
        press(&mut app, KeyCode::Char('K'));
        press_shift(&mut app, KeyCode::Up);
        match &app.view {
            View::Diff { scroll, .. } => assert_eq!(*scroll, 0),
            _ => panic!("expected diff view"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
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
        match &app.view {
            View::Diff {
                content, marked, ..
            } => {
                assert!(
                    content.contains("two"),
                    "shows the file's own diff: {content}"
                );
                assert!(marked.iter().all(|m| *m), "everything is marked by default");
            }
            _ => panic!("expected diff view"),
        }
        // Space unmarks the current file for commit.
        press(&mut app, KeyCode::Char(' '));
        match &app.view {
            View::Diff {
                files,
                marked,
                rows,
                selected,
                ..
            } => {
                let i = files.iter().position(|f| f.path == "f.txt").unwrap();
                assert_eq!(current_file_index(rows, *selected), Some(i));
                assert!(!marked[i], "space toggled the mark off");
            }
            _ => panic!("expected diff view"),
        }
        // Revert discards the change; f.txt returns to its committed content.
        press(&mut app, KeyCode::Char('R'));
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
        match &app.view {
            View::ConfirmExisting {
                existing_name,
                selected,
                ..
            } => {
                assert_eq!(existing_name.as_deref(), Some("spare"));
                assert_eq!(*selected, 0, "defaults to Open for a real worktree");
            }
            _ => panic!("expected the existing-directory prompt"),
        }
        // Enter opens the existing worktree's diff.
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Diff { name, .. } => assert_eq!(name, "spare"),
            _ => panic!("expected the diff view for the existing worktree"),
        }
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
        let len = match &app.view {
            View::Diff { rows, .. } => rows.len(),
            _ => panic!("expected diff view"),
        };
        assert_eq!(len, 2);
        app.row_list = Some(RowList {
            inner: Rect::new(0, 2, 30, 10),
            header: 0,
            offset: 0,
            len,
        });

        // Click the second row (y = inner.y + 1).
        click(&mut app, 1, 3);
        match &app.view {
            View::Diff {
                selected,
                rows,
                content,
                ..
            } => {
                assert_eq!(*selected, 1, "cursor moved to the clicked row");
                let i = current_file_index(rows, *selected).unwrap();
                assert_eq!(i, 1);
                assert!(
                    content.contains("22"),
                    "clicked file's diff loaded: {content}"
                );
            }
            _ => panic!("expected diff view"),
        }

        // A click outside the list rows leaves the selection untouched.
        click(&mut app, 1, 99);
        match &app.view {
            View::Diff { selected, .. } => assert_eq!(*selected, 1),
            _ => panic!("expected diff view"),
        }
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
        match &app.view {
            View::Diff {
                ignore_prompt: Some(p),
                ..
            } => {
                assert_eq!(p.file, "debug.log");
                assert_eq!(p.pattern, "*.log");
                assert_eq!(p.selected, 0);
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
        match &app.view {
            View::Diff { ignore_prompt, .. } => {
                assert!(ignore_prompt.is_none(), "prompt closed after confirming")
            }
            _ => panic!("expected diff view"),
        }
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
        match &app.view {
            View::Diff { ignore_prompt, .. } => assert!(ignore_prompt.is_none()),
            _ => panic!("expected diff view"),
        }

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
        match &app.view {
            View::Diff { content, .. } => assert!(content.contains("two"), "{content}"),
            _ => panic!("expected diff view"),
        }

        // A further outside edit is picked up when the user presses `r`.
        std::fs::write(root.join("file.txt"), "three\n").unwrap();
        press(&mut app, KeyCode::Char('r'));
        select_diff_file(&mut app, "file.txt");
        match &app.view {
            View::Diff { content, .. } => assert!(content.contains("three"), "{content}"),
            _ => panic!("expected diff view"),
        }

        // A further edit is picked up by tick once the throttle window passes.
        std::fs::write(root.join("file.txt"), "four\n").unwrap();
        if let View::Diff { last_refresh, .. } = &mut app.view {
            *last_refresh = Instant::now()
                .checked_sub(DIFF_REFRESH_INTERVAL * 2)
                .unwrap();
        }
        app.tick();
        select_diff_file(&mut app, "file.txt");
        match &app.view {
            View::Diff { content, .. } => assert!(content.contains("four"), "{content}"),
            _ => panic!("expected diff view"),
        }
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
        let before = match &app.view {
            View::Diff { scroll, .. } => *scroll,
            _ => panic!("expected diff view"),
        };
        assert_eq!(before, 6);
        if let View::Diff { last_refresh, .. } = &mut app.view {
            *last_refresh = Instant::now()
                .checked_sub(DIFF_REFRESH_INTERVAL * 2)
                .unwrap();
        }
        app.tick();
        match &app.view {
            View::Diff { scroll, .. } => {
                assert_eq!(*scroll, before, "auto-refresh must not reset scroll")
            }
            _ => panic!("expected diff view"),
        }
    }

    /// Moves the diff view's cursor onto the folder row whose prefix is
    /// `prefix`, panicking if it isn't in the list.
    fn select_diff_folder(app: &mut App, prefix: &str) {
        loop {
            match &app.view {
                View::Diff { rows, selected, .. } => {
                    if let Some(DiffRow::Folder { prefix: p, .. }) = rows.get(*selected)
                        && p == prefix
                    {
                        return;
                    }
                    assert!(*selected + 1 < rows.len(), "{prefix} not in the diff list");
                }
                _ => panic!("expected diff view"),
            }
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
        let rows = build_diff_rows(&files);
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
        match &app.view {
            View::Diff { files, marked, .. } => {
                for (f, m) in files.iter().zip(marked.iter()) {
                    if f.path.starts_with("pkg/") {
                        assert!(!m, "{} should be unmarked", f.path);
                    } else {
                        assert!(m, "{} should stay marked", f.path);
                    }
                }
            }
            _ => panic!("expected diff view"),
        }

        // Space again re-marks the whole folder.
        select_diff_folder(&mut app, "pkg/");
        press(&mut app, KeyCode::Char(' '));
        match &app.view {
            View::Diff { marked, .. } => assert!(marked.iter().all(|m| *m)),
            _ => panic!("expected diff view"),
        }
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
        match &app.view {
            View::Diff {
                ignore_prompt: Some(p),
                ..
            } => {
                assert!(p.is_folder);
                assert_eq!(p.file, "build/");
                assert_eq!(p.pattern, "build/");
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
        match &app.view {
            View::Diff { files, scroll, .. } => {
                assert!(
                    !files.iter().any(|f| f.path == "file.txt"),
                    "clean file leaves the changes list"
                );
                assert_eq!(*scroll, 0, "reload resets the scroll");
            }
            _ => panic!("expected diff view"),
        }
    }

    #[test]
    fn uninitialized_repo_opens_setup_wizard_and_esc_quits() {
        let (_tmp, mut app) = test_app_uninitialized();
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(
                    wizard.step,
                    super::setup::Step::CloneAsk { yes: false }
                ));
            }
            _ => panic!("expected the setup wizard"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }

    #[test]
    fn setup_manual_flow_writes_config_and_enters_list() {
        let (_tmp, mut app) = test_app_uninitialized();
        // Decline cloning, pick "inside" (second preset), copy .env, no
        // commands, then confirm on the review screen.
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, ".env");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter); // blank command list -> review
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::Review { .. }));
                assert_eq!(wizard.draft.worktree_dir, "inside");
                assert_eq!(wizard.draft.copy, vec![".env"]);
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

        // yes -> type the source repo path -> review shows the cloned draft.
        press(&mut app, KeyCode::Char('y'));
        type_str(&mut app, source.to_str().unwrap());
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Setup(wizard) => {
                assert!(matches!(wizard.step, super::setup::Step::Review { .. }));
                assert_eq!(wizard.draft.worktree_dir, "home");
                assert_eq!(wizard.draft.copy, vec![".env"]);
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

    #[test]
    fn setup_bad_clone_path_stays_on_input_with_error() {
        let (_tmp, mut app) = test_app_uninitialized();
        press(&mut app, KeyCode::Char('y')); // yes
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

        press(&mut app, KeyCode::Char('y')); // yes -> path input
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
        // Stash the current changes with a message.
        press(&mut app, KeyCode::Char('s'));
        type_str(&mut app, "wip");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        match &app.view {
            View::Stash { entries, .. } => assert_eq!(entries.len(), 1),
            _ => panic!("expected stash overlay"),
        }
        app.refresh();
        assert_eq!(app.worktrees[0].dirty, 0, "stash should clean the tree");

        // Pop it back.
        press(&mut app, KeyCode::Char('p'));
        settle(&mut app);
        match &app.view {
            View::Stash { entries, .. } => assert!(entries.is_empty()),
            _ => panic!("expected stash overlay"),
        }
        app.refresh();
        assert!(app.worktrees[0].dirty > 0, "pop restores the change");
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
            app.view,
            View::Stash {
                mode: StashMode::ConfirmDrop,
                ..
            }
        ));
        press(&mut app, KeyCode::Char('y'));
        settle(&mut app);
        match &app.view {
            View::Stash { entries, .. } => assert!(entries.is_empty(), "drop removes the entry"),
            _ => panic!("expected stash overlay"),
        }
    }

    #[test]
    fn branches_tab_creates_and_deletes_branches() {
        let (_tmp, mut app) = test_app();
        // Tab switches from the Worktrees tab to the Branches tab.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        // Create a new branch "feature".
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
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

    #[test]
    fn branches_tab_d_key_opens_confirm_delete() {
        let (_tmp, mut app) = test_app();
        // Tab switches from the Worktrees tab to the Branches tab.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        // The main branch is listed by default, so `d` has something to target.
        assert!(!app.branches.is_empty());
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.branch_mode, BranchMode::ConfirmDelete));
    }

    #[test]
    fn branches_tab_c_opens_prefilled_create() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "spare"]);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        app.branch_selected = app.branches.iter().position(|b| b.name == "spare").unwrap();
        // `c` checks out an existing branch, so the create dialog opens with
        // that branch selected in the checkout list.
        press(&mut app, KeyCode::Char('c'));
        match &app.view {
            View::Create {
                branches, selected, ..
            } => {
                assert!(*selected >= 1);
                assert_eq!(branches[*selected - 1], "spare");
            }
            _ => panic!("expected the create dialog prefilled with the branch"),
        }
    }

    #[test]
    fn branches_tab_enter_opens_commits_and_marks_for_cherry_pick() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.tab, Tab::Branches);
        // Enter on a branch drills into its commit history.
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::BranchCommits { entries, .. } => assert!(!entries.is_empty()),
            _ => panic!("expected the branch commits view"),
        }
        // Space marks the commit under the cursor, and Enter opens the
        // cherry-pick worktree picker with it selected.
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
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
                assert!(branches.iter().any(|b| b == "spare"));
                // The worktree's own current branch is not offered.
                assert!(!branches.iter().any(|b| b == "main" || b == "master"));
            }
            _ => panic!("expected the switch picker"),
        }
        // Select "spare" and switch onto it.
        if let View::Switch {
            branches, selected, ..
        } = &mut app.view
        {
            *selected = branches.iter().position(|b| b == "spare").unwrap();
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(app.worktrees.iter().any(|w| w.branch.as_deref() == Some("spare")));
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
        match &app.view {
            View::Switch {
                branches, filter, ..
            } => {
                assert_eq!(
                    filtered_branches(branches, filter.as_str()),
                    vec!["feature-auth", "feature-billing"]
                );
            }
            _ => panic!("expected the switch picker"),
        }
        // Narrowing further to a single match, Enter switches to that match
        // (not to an index into the full, unfiltered branch list).
        type_str(&mut app, "-billing");
        match &app.view {
            View::Switch {
                branches, filter, ..
            } => {
                assert_eq!(
                    filtered_branches(branches, filter.as_str()),
                    vec!["feature-billing"]
                );
            }
            _ => panic!("expected the switch picker"),
        }
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("feature-billing"))
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
        assert!(matches!(app.view, View::List), "second Esc closes the picker");
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
            View::Switch { filter, selected, .. } => {
                assert_eq!(filter.as_str(), "jk");
                assert_eq!(*selected, 0);
            }
            _ => panic!("expected the switch picker"),
        }
    }

    #[test]
    fn log_overlay_opens_and_scrolls() {
        let (_tmp, mut app) = test_app();
        app.selected = 0;
        press(&mut app, KeyCode::Char('l'));
        match &app.view {
            View::Log { entries, .. } => assert!(!entries.is_empty()),
            _ => panic!("expected log overlay"),
        }
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::PageDown);
        match &app.view {
            View::Log { scroll, .. } => assert_eq!(*scroll, 21),
            _ => panic!("expected log overlay"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
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
        match &app.view {
            View::ConfirmDelete {
                delete_branch,
                branch,
                ..
            } => {
                assert!(!delete_branch, "folder-only must be the default");
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
        match &app.view {
            View::ConfirmDelete { delete_branch, .. } => assert!(delete_branch),
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
            matches!(app.view, View::ConfirmDeleteDirty { .. }),
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
        match &app.view {
            View::ConfirmForceBranch { branch, reason } => {
                assert_eq!(branch, "feature");
                assert!(matches!(reason, ForceBranchReason::NotMerged));
            }
            _ => panic!("expected the force-branch prompt after an unmerged branch delete"),
        }
        assert!(!app.worktrees.iter().any(|w| w.name == "feature"));
        assert!(crate::git::branch_exists(&app.ctx.repo_root, "feature"));
        // Force the delete.
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.view, View::List));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "feature"));
    }

    #[test]
    fn config_editor_edits_and_saves_settings() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('o'));
        assert!(matches!(app.view, View::Config(_)));

        // Edit worktree_dir (row 0): clear, type "inside".
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "inside");
        press(&mut app, KeyCode::Enter);
        // Move past open_command (row 1) to setup.copy (row 2) and set it.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, ".env, config/.env.local");
        press(&mut app, KeyCode::Enter);
        // Down to setup.run (3) then to save row (4) and save.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);

        assert!(matches!(app.view, View::List), "message: {:?}", app.message);
        assert!(app.message.as_deref().unwrap().contains("saved"));
        // The live config reflects the change without a reload.
        assert_eq!(app.ctx.config.worktree_dir.as_deref(), Some("inside"));
        let text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(text.contains("worktree_dir = \"inside\""), "{text}");
        assert!(text.contains(".env"), "{text}");
        assert!(text.contains("config/.env.local"), "{text}");
    }

    #[test]
    fn config_editor_clearing_a_field_unsets_it() {
        let (_tmp, mut app) = test_app();
        std::fs::write(
            app.ctx.repo_root.join(".wtm.toml"),
            "worktree_dir = \"home\"\n[setup]\ncopy = [\".env\"]\n",
        )
        .unwrap();

        press(&mut app, KeyCode::Char('o'));
        // Row 0 (worktree_dir) should load the existing "home".
        match &app.view {
            View::Config(editor) => assert_eq!(editor.worktree_dir, "home"),
            _ => panic!("expected config editor"),
        }
        // Clear worktree_dir back to empty.
        press(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            press(&mut app, KeyCode::Backspace);
        }
        press(&mut app, KeyCode::Enter);
        // Save (down past open_command, setup.copy, setup.run to the save row).
        for _ in 0..4 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);

        assert!(matches!(app.view, View::List));
        let text = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert!(!text.contains("worktree_dir"), "should be unset: {text}");
        assert!(text.contains(".env"), "copy should remain: {text}");
    }

    #[test]
    fn config_editor_cancel_leaves_file_untouched() {
        let (_tmp, mut app) = test_app();
        let before = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "home");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc); // cancel without saving
        assert!(matches!(app.view, View::List));
        let after = std::fs::read_to_string(app.ctx.repo_root.join(".wtm.toml")).unwrap();
        assert_eq!(before, after, "cancel must not write the file");
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
            draw(&mut app); // diff
            press(&mut app, KeyCode::Esc);
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

            // Config editor: navigating and mid-edit.
            press(&mut app, KeyCode::Char('o'));
            draw(&mut app);
            press(&mut app, KeyCode::Enter); // edit worktree_dir
            type_str(&mut app, "inside");
            draw(&mut app);
            press(&mut app, KeyCode::Esc); // cancel edit
            press(&mut app, KeyCode::Esc); // close editor

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

            // Stash overlay and its sub-modes.
            press(&mut app, KeyCode::Char('s'));
            draw(&mut app); // stash list (empty)
            press(&mut app, KeyCode::Char('s'));
            type_str(&mut app, "msg");
            draw(&mut app); // stash message input
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Char('x'));
            draw(&mut app); // drop confirm
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);

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
}
