//! TUI application state and key handling.

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::config_editor::{ConfigEditor, EditorOutcome};
use super::setup::{self, SetupWizard, WizardOutcome};
use crate::git::{LogEntry, StashEntry, StatusEntry};
use crate::ops::{self, BranchListItem, Ctx, SetupControl, WorktreeInfo};
use crate::settings::ConfigDraft;

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
        /// Cursor into `files`.
        selected: usize,
        /// Diff text for `files[selected]`.
        content: String,
        scroll: u16,
        /// When the diff was last recomputed, used to throttle auto-refresh.
        last_refresh: Instant,
        /// True while confirming a revert of the highlighted file.
        confirm_revert: bool,
    },
    /// New-worktree dialog: type a branch name, or pick an existing branch
    /// from the filtered list below the input.
    Create {
        input: String,
        /// Local branches not already checked out, newest commit first.
        branches: Vec<String>,
        /// 0 = create the branch typed in `input`; 1..=n = the n-th entry of
        /// the currently filtered branch list.
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
    Help,
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
    /// Branch browser across the whole repo.
    Branch {
        branches: Vec<BranchListItem>,
        selected: usize,
        mode: BranchMode,
    },
    /// Scrollable commit log for one worktree.
    Log {
        name: String,
        entries: Vec<LogEntry>,
        scroll: u16,
    },
    /// A git operation (pull/push/fetch) running on a background thread. Its
    /// result message is shown and the list refreshed when it finishes.
    Busy {
        label: String,
        rx: Receiver<Result<String, String>>,
    },
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

/// Sub-state of the branch overlay.
pub enum BranchMode {
    List,
    /// Typing a name for a new branch.
    Create(String),
    /// Confirming deletion of the selected branch (`f` forces on refusal).
    ConfirmDelete,
}

pub struct App {
    pub ctx: Ctx,
    pub worktrees: Vec<WorktreeInfo>,
    pub selected: usize,
    pub view: View,
    /// One-line status or error shown in the header. Auto-clears after a few
    /// seconds so it doesn't linger over the key hints.
    pub message: Option<String>,
    /// When the current `message` first appeared, plus the text it was set for,
    /// so a replaced message restarts the timer. Managed by `expire_message`.
    message_at: Option<Instant>,
    message_shown: Option<String>,
    /// Where new worktrees will be created, shown in the create dialog.
    pub worktree_base: Option<String>,
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
            view,
            message: None,
            message_at: None,
            message_shown: None,
            worktree_base,
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
    }

    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.worktrees.get(self.selected)
    }

    /// Background work driven by the event loop's poll timeout: auto-refreshes
    /// the diff view and drains progress from an in-flight create.
    pub fn tick(&mut self) {
        self.expire_message();
        if let View::Busy { rx, .. } = &self.view {
            if let Ok(result) = rx.try_recv() {
                self.message = Some(match result {
                    Ok(m) => m,
                    Err(e) => format!("error: {e}"),
                });
                self.view = View::List;
                self.refresh();
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
        match &mut self.view {
            View::List => self.on_list_key(key),
            View::Diff { .. } => self.on_diff_key(key),
            View::Create {
                input,
                branches,
                selected,
            } => match key.code {
                KeyCode::Esc => self.view = View::List,
                KeyCode::Down => {
                    let max = filtered_branches(branches, input).len();
                    if *selected < max {
                        *selected += 1;
                    }
                }
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Enter => {
                    let branch = if *selected == 0 {
                        input.trim().to_string()
                    } else {
                        filtered_branches(branches, input)[*selected - 1].clone()
                    };
                    if !branch.is_empty() {
                        self.start_create(branch);
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                    *selected = 0;
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    *selected = 0;
                }
                _ => {}
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
            View::ConfirmDelete {
                name,
                dirty,
                branch,
                delete_branch,
            } => match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    // Detached worktrees have no branch to offer deleting.
                    if branch.is_some() {
                        *delete_branch = !*delete_branch;
                    }
                }
                KeyCode::Enter | KeyCode::Char('y') if *dirty == 0 => {
                    let (name, del) = (name.clone(), *delete_branch);
                    self.remove(&name, false, del);
                }
                KeyCode::Char('f') => {
                    let (name, del) = (name.clone(), *delete_branch);
                    self.remove(&name, true, del);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.view = View::List,
                _ => {}
            },
            View::Help => self.view = View::List,
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
            View::Branch { .. } => self.on_branch_key(key),
            View::Log { .. } => self.on_log_key(key),
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
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
            KeyCode::Char('c') => match ConfigEditor::load(self.ctx.repo_root.clone()) {
                Ok(editor) => self.view = View::Config(Box::new(editor)),
                Err(e) => self.message = Some(format!("error: {e:#}")),
            },
            KeyCode::Char('C') => self.open_commit(),
            KeyCode::Char('s') => self.open_stash(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.start_push(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('b') => self.open_branch(),
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
            KeyCode::Char('?') => self.view = View::Help,
            _ => {}
        }
    }

    /// Opens the per-file changes view for the worktree named `name`.
    fn open_diff(&mut self, name: String) {
        match ops::status(&self.ctx, &name) {
            Ok((_, files)) => {
                let marked = vec![true; files.len()];
                self.view = View::Diff {
                    name,
                    files,
                    marked,
                    selected: 0,
                    content: String::new(),
                    scroll: 0,
                    last_refresh: Instant::now(),
                    confirm_revert: false,
                };
                self.load_diff_content();
            }
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
    }

    /// Loads the diff text for the file under the cursor into the Diff view.
    fn load_diff_content(&mut self) {
        let View::Diff {
            name,
            files,
            selected,
            ..
        } = &self.view
        else {
            return;
        };
        let (name, entry) = (name.clone(), files.get(*selected).cloned());
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
            *scroll = 0;
        }
    }

    fn on_diff_key(&mut self, key: KeyEvent) {
        let View::Diff {
            files,
            marked,
            selected,
            confirm_revert,
            ..
        } = &mut self.view
        else {
            return;
        };
        if *confirm_revert {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let entry = files.get(*selected).cloned();
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
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
            KeyCode::Char('r') => self.refresh_diff(),
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected + 1 < files.len() {
                    *selected += 1;
                    self.load_diff_content();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if *selected > 0 {
                    *selected -= 1;
                    self.load_diff_content();
                }
            }
            KeyCode::PageDown => self.scroll_diff(|s| s.saturating_add(20)),
            KeyCode::PageUp => self.scroll_diff(|s| s.saturating_sub(20)),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_diff(|_| 0),
            KeyCode::Char(' ') => {
                if let Some(m) = marked.get_mut(*selected) {
                    *m = !*m;
                }
            }
            KeyCode::Char('a') => {
                let all_on = marked.iter().all(|m| *m);
                marked.iter_mut().for_each(|m| *m = !all_on);
            }
            KeyCode::Char('s') => {
                if let Some(e) = files.get(*selected).cloned() {
                    self.stash_file(e);
                }
            }
            KeyCode::Char('R') => {
                if !files.is_empty() {
                    *confirm_revert = true;
                }
            }
            KeyCode::Char('C') => self.commit_from_diff(),
            _ => {}
        }
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
        match ops::status(&self.ctx, &name) {
            Ok((_, new_files)) => {
                if let View::Diff {
                    files,
                    marked,
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
                    *files = new_files;
                    *marked = new_marked;
                    *selected = (*selected).min(files.len().saturating_sub(1));
                    *last_refresh = Instant::now();
                }
                self.load_diff_content();
            }
            // The worktree may have been removed out from under us; surface it
            // and drop back to the list rather than looping on the error.
            Err(e) => {
                self.message = Some(format!("error: {e:#}"));
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
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

    /// Opens the new-worktree dialog, offering existing local branches that
    /// aren't already checked out somewhere.
    fn open_create(&mut self) {
        let checked_out: Vec<&str> = self
            .worktrees
            .iter()
            .filter_map(|w| w.branch.as_deref())
            .collect();
        let branches = match crate::git::local_branches(&self.ctx.repo_root) {
            Ok(all) => all
                .into_iter()
                .filter(|b| !checked_out.contains(&b.as_str()))
                .collect(),
            Err(e) => {
                self.message = Some(format!("error: {e:#}"));
                return;
            }
        };
        self.view = View::Create {
            input: String::new(),
            branches,
            selected: 0,
        };
    }

    /// Kicks off `ops::create` on a background thread so setup commands
    /// (npm install etc.) don't freeze the UI.
    fn start_create(&mut self, branch: String) {
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
                None,
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
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
        match ops::commit(&self.ctx, &name, &message, Some(&paths)) {
            Ok(r) => {
                self.message = Some(format!(
                    "committed {} · {} ({} file{})",
                    r.hash,
                    r.summary,
                    r.files_changed,
                    if r.files_changed == 1 { "" } else { "s" }
                ));
                self.view = View::List;
                self.refresh();
            }
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
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
                self.message = Some(format!("error: {e:#}"));
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
        let result = match action {
            "pop" => ops::stash_pop(&self.ctx, &name, index),
            "apply" => ops::stash_apply(&self.ctx, &name, index),
            _ => ops::stash_drop(&self.ctx, &name, index),
        };
        match result {
            Ok(r) => self.message = Some(format!("stash {} on '{}'", r.action, r.name)),
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
        self.refresh();
        self.load_stash(name, StashMode::List);
    }

    /// Stashes the worktree's current changes with an optional message.
    fn stash_push(&mut self, name: String, message: Option<String>) {
        match ops::stash_push(&self.ctx, &name, message.as_deref()) {
            Ok(_) => self.message = Some(format!("stashed changes in '{name}'")),
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
        self.refresh();
        self.load_stash(name, StashMode::List);
    }

    /// Opens the branch browser.
    fn open_branch(&mut self) {
        self.load_branches(BranchMode::List, 0);
    }

    /// (Re)loads all local branches into the branch overlay, clamping the
    /// selection. Falls back to the list view on error.
    fn load_branches(&mut self, mode: BranchMode, selected: usize) {
        match ops::branch_list(&self.ctx) {
            Ok(r) => {
                let selected = selected.min(r.branches.len().saturating_sub(1));
                self.view = View::Branch {
                    branches: r.branches,
                    selected,
                    mode,
                };
            }
            Err(e) => {
                self.message = Some(format!("error: {e:#}"));
                self.view = View::List;
            }
        }
    }

    fn on_branch_key(&mut self, key: KeyEvent) {
        let View::Branch {
            branches,
            selected,
            mode,
        } = &mut self.view
        else {
            return;
        };
        match mode {
            BranchMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected + 1 < branches.len() {
                        *selected += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Char('n') => *mode = BranchMode::Create(String::new()),
                KeyCode::Char('x') => {
                    if !branches.is_empty() {
                        *mode = BranchMode::ConfirmDelete;
                    }
                }
                KeyCode::Enter => {
                    if let Some(b) = branches.get(*selected) {
                        if b.checked_out_path.is_some() {
                            self.message =
                                Some(format!("branch '{}' is already checked out", b.name));
                        } else {
                            let branch = b.name.clone();
                            self.open_create_prefilled(branch);
                        }
                    }
                }
                _ => {}
            },
            BranchMode::Create(buf) => match key.code {
                KeyCode::Esc => *mode = BranchMode::List,
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
            },
            BranchMode::ConfirmDelete => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    if let Some(name) = branches.get(*selected).map(|b| b.name.clone()) {
                        self.branch_delete(name, false);
                    }
                }
                KeyCode::Char('f') => {
                    if let Some(name) = branches.get(*selected).map(|b| b.name.clone()) {
                        self.branch_delete(name, true);
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => *mode = BranchMode::List,
                _ => {}
            },
        }
    }

    /// Creates a branch from HEAD and reloads the browser.
    fn branch_create(&mut self, name: String) {
        match ops::branch_create(&self.ctx, &name, None) {
            Ok(_) => self.message = Some(format!("created branch '{name}'")),
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
        self.load_branches(BranchMode::List, 0);
    }

    /// Deletes a branch. A refused non-force delete keeps the confirm open so
    /// the user can retry with `f` (force).
    fn branch_delete(&mut self, name: String, force: bool) {
        match ops::branch_delete(&self.ctx, &name, force) {
            Ok(r) => {
                self.message = Some(format!(
                    "deleted branch '{}'{}",
                    r.name,
                    if r.forced { " (forced)" } else { "" }
                ));
                self.load_branches(BranchMode::List, 0);
            }
            Err(e) => self.message = Some(format!("error: {e:#} — press f to force")),
        }
    }

    /// Opens the new-worktree dialog prefilled with `branch`, used when the
    /// branch browser targets a branch that isn't checked out anywhere.
    fn open_create_prefilled(&mut self, branch: String) {
        self.open_create();
        if let View::Create { input, .. } = &mut self.view {
            *input = branch;
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
            Err(e) => self.message = Some(format!("error: {e:#}")),
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
            _ => {}
        }
    }

    /// Runs `op` on a background thread and shows the Busy overlay until
    /// tick() drains its result. Keeps long git ops off the UI thread.
    fn start_busy(
        &mut self,
        label: String,
        op: impl FnOnce(&Ctx) -> Result<String, String> + Send + 'static,
    ) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(op(&ctx));
        });
        self.view = View::Busy { label, rx };
    }

    /// Pulls the selected worktree (fast-forward only) in the background.
    fn start_pull(&mut self) {
        let Some(wt) = self.selected_worktree() else {
            return;
        };
        let name = wt.name.clone();
        self.start_busy(format!("pulling {name}…"), move |ctx| {
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
        self.start_busy(format!("pushing {name}…"), move |ctx| {
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
        self.start_busy("fetching all remotes…".to_string(), move |ctx| {
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

    fn remove(&mut self, name: &str, force: bool, delete_branch: bool) {
        match ops::remove(&self.ctx, name, force, delete_branch) {
            Ok(info) => {
                self.message = Some(match (&info.branch, delete_branch) {
                    (Some(b), true) => format!("removed '{}' and branch '{b}'", info.name),
                    (Some(_), false) => format!("removed '{}' (branch kept)", info.name),
                    (None, _) => format!("removed '{}'", info.name),
                });
            }
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
        self.view = View::List;
        self.refresh();
    }
}

/// Branches matching the typed filter (case-insensitive substring),
/// preserving their recency order.
pub fn filtered_branches<'a>(branches: &'a [String], filter: &str) -> Vec<&'a String> {
    let needle = filter.to_lowercase();
    branches
        .iter()
        .filter(|b| b.to_lowercase().contains(&needle))
        .collect()
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

    fn ctrl_c(app: &mut App) {
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    /// Moves the diff view's cursor onto the file named `path`, panicking if it
    /// isn't in the list.
    fn select_diff_file(app: &mut App, path: &str) {
        loop {
            match &app.view {
                View::Diff {
                    files, selected, ..
                } => {
                    if files[*selected].path == path {
                        return;
                    }
                    assert!(*selected + 1 < files.len(), "{path} not in the diff list");
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
        assert!(matches!(app.view, View::Help));
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(app.view, View::List));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn create_dialog_collects_input_and_cancels() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Backspace);
        match &app.view {
            View::Create { input, .. } => assert_eq!(input, "a"),
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
        // Typing filters the list; ↓ selects the surviving branch.
        type_str(&mut app, "spa");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Creating { branch, .. } => assert_eq!(branch, "spare"),
            _ => panic!("expected creating view"),
        }
        wait_creating(&mut app, |_, done| done);
        press(&mut app, KeyCode::Enter);
        assert!(app.worktrees.iter().any(|w| w.name == "spare"));
    }

    #[test]
    fn filtered_branches_matches_case_insensitively() {
        let branches = vec!["Feature/Login".to_string(), "bugfix".to_string()];
        assert_eq!(filtered_branches(&branches, "log").len(), 1);
        assert_eq!(filtered_branches(&branches, "").len(), 2);
        assert!(filtered_branches(&branches, "zzz").is_empty());
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
        press(&mut app, KeyCode::PageDown);
        match &app.view {
            View::Diff { scroll, .. } => assert_eq!(*scroll, 20),
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
                selected,
                ..
            } => {
                let i = files.iter().position(|f| f.path == "f.txt").unwrap();
                assert_eq!(*selected, i);
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
        press(&mut app, KeyCode::PageDown); // scroll to 20
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
        press(&mut app, KeyCode::Char('C'));
        assert!(matches!(app.view, View::Commit { .. }));
        type_str(&mut app, "add scratch");
        press(&mut app, KeyCode::Enter);
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
        press(&mut app, KeyCode::Char('C'));
        assert!(matches!(app.view, View::List));
        assert!(app.message.as_deref().unwrap().contains("clean"));
    }

    #[test]
    fn commit_empty_message_is_rejected() {
        let (_tmp, mut app) = test_app();
        dirty_main(&mut app);
        press(&mut app, KeyCode::Char('C'));
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
        match &app.view {
            View::Stash { entries, .. } => assert_eq!(entries.len(), 1),
            _ => panic!("expected stash overlay"),
        }
        app.refresh();
        assert_eq!(app.worktrees[0].dirty, 0, "stash should clean the tree");

        // Pop it back.
        press(&mut app, KeyCode::Char('p'));
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
        press(&mut app, KeyCode::Char('x')); // arm drop
        assert!(matches!(
            app.view,
            View::Stash {
                mode: StashMode::ConfirmDrop,
                ..
            }
        ));
        press(&mut app, KeyCode::Char('y'));
        match &app.view {
            View::Stash { entries, .. } => assert!(entries.is_empty(), "drop removes the entry"),
            _ => panic!("expected stash overlay"),
        }
    }

    #[test]
    fn branch_browser_creates_and_deletes_branches() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('b'));
        assert!(matches!(app.view, View::Branch { .. }));
        // Create a new branch "feature".
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "feature");
        press(&mut app, KeyCode::Enter);
        assert!(crate::git::branch_exists(&app.ctx.repo_root, "feature"));
        match &app.view {
            View::Branch { branches, .. } => {
                assert!(branches.iter().any(|b| b.name == "feature"));
            }
            _ => panic!("expected branch overlay"),
        }
        // Select "feature" and delete it (main is not deletable while checked out).
        let idx = match &app.view {
            View::Branch { branches, .. } => {
                branches.iter().position(|b| b.name == "feature").unwrap()
            }
            _ => unreachable!(),
        };
        if let View::Branch { selected, .. } = &mut app.view {
            *selected = idx;
        }
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('y'));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "feature"));
    }

    #[test]
    fn branch_enter_opens_prefilled_create() {
        let (_tmp, mut app) = test_app();
        git(&app.ctx.repo_root, &["branch", "spare"]);
        press(&mut app, KeyCode::Char('b'));
        let idx = match &app.view {
            View::Branch { branches, .. } => {
                branches.iter().position(|b| b.name == "spare").unwrap()
            }
            _ => panic!("expected branch overlay"),
        };
        if let View::Branch { selected, .. } = &mut app.view {
            *selected = idx;
        }
        press(&mut app, KeyCode::Enter);
        match &app.view {
            View::Create { input, .. } => assert_eq!(input, "spare"),
            _ => panic!("expected the create dialog prefilled with the branch"),
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
        assert!(app.message.as_deref().unwrap().contains("no upstream"));
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
        assert!(!app.worktrees.iter().any(|w| w.name == "dropme"));
        assert!(!crate::git::branch_exists(&app.ctx.repo_root, "dropme"));
    }

    #[test]
    fn config_editor_edits_and_saves_settings() {
        let (_tmp, mut app) = test_app();
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.view, View::Config(_)));

        // Edit worktree_dir (row 0): clear, type "inside".
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "inside");
        press(&mut app, KeyCode::Enter);
        // Move to setup.copy (row 1) and set it.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, ".env, config/.env.local");
        press(&mut app, KeyCode::Enter);
        // Down to setup.run (2) then to save row (3) and save.
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

        press(&mut app, KeyCode::Char('c'));
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
        // Save.
        for _ in 0..3 {
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
        press(&mut app, KeyCode::Char('c'));
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
            draw(&mut app); // create dialog with a filtered branch list
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('d'));
            draw(&mut app); // delete dialog
            press(&mut app, KeyCode::Down);
            draw(&mut app); // delete dialog, branch option selected
            press(&mut app, KeyCode::Esc);

            // Config editor: navigating and mid-edit.
            press(&mut app, KeyCode::Char('c'));
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
            press(&mut app, KeyCode::Char('C'));
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

            // Branch overlay and its sub-modes.
            press(&mut app, KeyCode::Char('b'));
            draw(&mut app); // branch list
            press(&mut app, KeyCode::Char('n'));
            type_str(&mut app, "feat2");
            draw(&mut app); // create-branch input
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Char('x'));
            draw(&mut app); // delete confirm
            press(&mut app, KeyCode::Esc);
            press(&mut app, KeyCode::Esc);

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
