//! In-app editor for the repo's `.wtm.toml`, backing the Settings tab so
//! settings can be changed without editing the file by hand.
//!
//! It shows the repo-level settings as editable rows, the update-check toggle,
//! the diff-theme cycle (with a live colour sample), the Worktrees-tab layout
//! cycle, and a "check for updates now" row. Every change is written
//! immediately (text fields on Enter, cycle rows on Enter/Space, the
//! open_command / setup.copy / setup.run lists on `[ done ]`), preserving
//! comments and only writing the keys that are set; a cleared field unsets
//! that key so the default (or global value) applies again.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::TextInput;
use super::highlight::{self, DIFF_THEMES};
use crate::config::{self, OpenCommand};
use crate::settings::{self, RepoConfigFields};

/// Rows holding an editable value (worktree_dir, open_command, setup.copy,
/// setup.run). Enter on one of these opens the text input, except the list
/// rows ([`OPEN_COMMAND_ROW`], [`COPY_ROW`], [`RUN_ROW`]), which open a
/// list editor.
pub const TEXT_ROWS: usize = 4;
/// Index of the `open_command` row, whose value is a list of commands the
/// Worktrees-tab `o` key can run rather than one string.
pub const OPEN_COMMAND_ROW: usize = 1;
/// Index of the `setup.copy` row, edited as a list of files.
pub const COPY_ROW: usize = 2;
/// Index of the `setup.run` row, edited as a list of commands.
pub const RUN_ROW: usize = 3;
/// Index of the update-check toggle, which Enter cycles rather than edits.
pub const UPDATE_ROW: usize = TEXT_ROWS;
/// Index of the diff-theme cycle row.
pub const THEME_ROW: usize = TEXT_ROWS + 1;
/// Index of the Worktrees-tab layout cycle row.
pub const LAYOUT_ROW: usize = TEXT_ROWS + 2;
/// Index of the Branches-tab cache timeout (editable minutes).
pub const BRANCHES_REFRESH_ROW: usize = TEXT_ROWS + 3;
/// Index of the diff line-number gutter toggle.
pub const DIFF_LINE_NUMBERS_ROW: usize = TEXT_ROWS + 4;
/// Number of setting rows, text fields plus the cycle rows, the Branches
/// refresh timeout, and the diff line-number toggle.
pub const FIELD_ROWS: usize = TEXT_ROWS + 5;
/// Index of the "check for updates now" row.
pub const CHECK_ROW: usize = FIELD_ROWS;
/// Total selectable rows.
pub const ROWS: usize = FIELD_ROWS + 1;

// Line layout for the Settings form. Each setting is a small block (value,
// description, blank separator) so neighbouring settings stay visually
// distinct. Repo settings and global settings each sit under a section
// header. The theme block inserts a live colour sample between its
// description and blank. Keep [`line_of_row`] / [`row_at_line`] as the single
// source of truth so clicks and draws cannot drift apart.

/// Section header plus the blank line under it, before the first setting.
const SECTION_HEADER_LINES: usize = 2;
/// Ordinary setting: value, description, trailing blank.
const SETTING_LINES: usize = 3;
/// Number of sample lines drawn under the theme preview label.
/// Kept in sync with `highlight::THEME_PREVIEW_SAMPLE`'s line count.
pub const THEME_PREVIEW_SAMPLE_LINES: usize = 3;

/// Height of one setting block, including its trailing blank.
fn setting_block_height(row: usize) -> usize {
    if row == THEME_ROW {
        // value + description + preview label + samples + blank
        SETTING_LINES + 1 + THEME_PREVIEW_SAMPLE_LINES
    } else {
        SETTING_LINES
    }
}

/// Form line of a field row's value (the selectable line).
pub fn line_of_row(row: usize) -> usize {
    let mut line = SECTION_HEADER_LINES; // "This repo" header + blank
    for r in 0..FIELD_ROWS {
        if r == UPDATE_ROW {
            line += SECTION_HEADER_LINES; // "All repos" header + blank
        }
        if r == row {
            return line;
        }
        line += setting_block_height(r);
    }
    // CHECK_ROW is past the fields; callers use CHECK_LINE for that.
    line
}

/// First form line after every setting block (worktree-location preview).
fn footer_start() -> usize {
    let mut line = SECTION_HEADER_LINES;
    for r in 0..FIELD_ROWS {
        if r == UPDATE_ROW {
            line += SECTION_HEADER_LINES;
        }
        line += setting_block_height(r);
    }
    line
}

/// Label line above the theme colour sample ("diff colours look like").
#[cfg(test)]
pub fn theme_preview_label_line() -> usize {
    line_of_row(THEME_ROW) + 2
}

/// First sample line of the theme colour preview.
#[cfg(test)]
pub fn theme_preview_line() -> usize {
    theme_preview_label_line() + 1
}

/// Line showing where worktrees will actually be created.
pub fn preview_line() -> usize {
    footer_start()
}

/// Line showing the running version and any update found.
#[cfg(test)]
pub fn version_line() -> usize {
    footer_start() + 1
}

/// Line of the "check for updates now" action.
pub fn check_line() -> usize {
    footer_start() + 2
}

/// Total lines the form occupies.
pub fn form_lines() -> usize {
    check_line() + 1
}

/// The row a click on form line `line` selects, or `None` for descriptions,
/// blanks, section headers, the theme sample, preview, and version lines.
pub fn row_at_line(line: usize) -> Option<usize> {
    if line == check_line() {
        return Some(CHECK_ROW);
    }
    (0..FIELD_ROWS).find(|&row| line_of_row(row) == line)
}

/// State of the Settings tab's editor.
pub struct ConfigEditor {
    pub repo_root: PathBuf,
    /// The global config file `auto_update_check`, `diff_theme`,
    /// `worktrees_layout`, and `branches_refresh_mins` are read from and
    /// written to, resolved once at load so a save cannot land somewhere else.
    /// `None` on a system with no locatable global config.
    pub global_config: Option<PathBuf>,
    /// The setting values as shown, each empty when unset.
    pub fields: RepoConfigFields,
    /// Selected row: 0..FIELD_ROWS edit a setting, then check-now.
    pub selected: usize,
    /// Cursor-aware buffer while editing the selected row; `None` when
    /// navigating. Shares `TextInput` with the other prompts so `←/→`,
    /// Home/End, and mid-string edits work here too.
    pub editing: Option<TextInput>,
    /// The `open_command` list editor while it is open, working on a copy of
    /// the list so cancelling discards its edits.
    pub open_list: Option<OpenCommandEditor>,
    /// The `setup.copy` / `setup.run` list editor while it is open.
    pub string_list: Option<StringListEditor>,
}

/// The `open_command` list editor: a modal over the Settings tab for adding,
/// editing, and removing individual commands. It works on a copy of the list
/// and only hands it back when the user confirms.
///
/// Rows are the commands, then an "add" row, then a "done" row, so every
/// action is reachable by arrow keys as well as by its shortcut. Each command
/// row carries two toggles beside its text: `g` saves the command globally
/// (every repo offers it) and `t` switches it between running in the
/// background and taking over the terminal.
pub struct OpenCommandEditor {
    /// Working copy of the list, one entry per configured command.
    pub commands: Vec<OpenCommand>,
    /// Cursor over the rows: `0..commands.len()`, then add, then done.
    pub selected: usize,
    /// Buffer while typing a command; `None` while navigating.
    pub input: Option<TextInput>,
    /// Which entry `input` is rewriting, or `None` when it is a new one.
    pub editing_index: Option<usize>,
}

/// What a key press did to the list editor.
enum ListOutcome {
    Continue,
    /// The user confirmed; the caller copies `commands` back into the fields.
    Done,
    /// The user cancelled; the working copy is dropped.
    Cancel,
}

impl OpenCommandEditor {
    /// Opens the editor on a copy of `commands`, with the first row selected.
    pub fn new(commands: Vec<OpenCommand>) -> OpenCommandEditor {
        OpenCommandEditor {
            commands,
            selected: 0,
            input: None,
            editing_index: None,
        }
    }

    /// Index of the "add a command" row, just past the commands.
    pub fn add_row(&self) -> usize {
        self.commands.len()
    }

    /// Index of the "done" row, the last one.
    pub fn done_row(&self) -> usize {
        self.commands.len() + 1
    }

    /// Total selectable rows: the commands plus add and done.
    pub fn rows(&self) -> usize {
        self.commands.len() + 2
    }

    /// Starts typing a new command, appended when committed.
    fn start_add(&mut self) {
        self.selected = self.add_row();
        self.editing_index = None;
        self.input = Some(TextInput::default());
    }

    /// Starts rewriting the command at `index`.
    fn start_edit(&mut self, index: usize) {
        self.editing_index = Some(index);
        self.input = Some(TextInput::with_value(&self.commands[index].command));
    }

    /// Flips whether the command under the cursor is saved globally, so the
    /// same toggle works while creating (the new entry is selected) and while
    /// editing an existing one.
    fn toggle_global(&mut self) {
        if let Some(cmd) = self.commands.get_mut(self.selected) {
            cmd.global = !cmd.global;
        }
    }

    /// Flips the command under the cursor between running in the background
    /// and taking over the terminal.
    fn toggle_mode(&mut self) {
        if let Some(cmd) = self.commands.get_mut(self.selected) {
            cmd.mode = cmd.mode.toggled();
        }
    }

    /// Drops the command at `index`, keeping the cursor on a valid row.
    fn remove(&mut self, index: usize) {
        self.commands.remove(index);
        self.selected = self.selected.min(self.rows() - 1);
    }

    /// Handles one key press, whether typing a command or navigating rows.
    fn on_key(&mut self, key: KeyEvent) -> ListOutcome {
        // Typing takes every key: Esc abandons the entry, Enter commits it,
        // and a blank entry is treated as "never mind" rather than an empty
        // command, since an empty command has nothing to run.
        if let Some(mut input) = self.input.take() {
            match key.code {
                KeyCode::Esc => self.editing_index = None,
                KeyCode::Enter => {
                    let value = input.trimmed();
                    match (self.editing_index.take(), value.is_empty()) {
                        (Some(index), false) => self.commands[index].command = value,
                        (None, false) => {
                            self.commands.push(OpenCommand::new(value));
                            // Land the cursor on the entry just created so its
                            // global/terminal toggles are right there.
                            self.selected = self.commands.len() - 1;
                        }
                        _ => {}
                    }
                }
                _ => {
                    input.on_key(key);
                    self.input = Some(input);
                }
            }
            return ListOutcome::Continue;
        }
        match key.code {
            KeyCode::Esc => return ListOutcome::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows() - 1)
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter if self.selected == self.done_row() => return ListOutcome::Done,
            KeyCode::Enter if self.selected == self.add_row() => self.start_add(),
            KeyCode::Enter => self.start_edit(self.selected),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char('d') | KeyCode::Delete if self.selected < self.commands.len() => {
                self.remove(self.selected)
            }
            KeyCode::Char('g') if self.selected < self.commands.len() => self.toggle_global(),
            KeyCode::Char('t') if self.selected < self.commands.len() => self.toggle_mode(),
            _ => {}
        }
        ListOutcome::Continue
    }
}

/// Which string-list setting the modal is editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringListKind {
    Copy,
    Run,
}

impl StringListKind {
    /// Dialog title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Copy => "copy files",
            Self::Run => "setup commands",
        }
    }

    /// One-line heading under the title.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Copy => "copied into each new worktree from the repo root",
            Self::Run => "run in each new worktree after create",
        }
    }

    /// Label on the add row while nothing is being typed.
    pub fn add_label(self) -> &'static str {
        match self {
            Self::Copy => "add a file",
            Self::Run => "add a command",
        }
    }
}

/// List editor for `setup.copy` and `setup.run`: a modal over the Settings
/// tab for adding, editing, and removing individual entries. Same shape as
/// [`OpenCommandEditor`] without the per-command global/terminal toggles.
pub struct StringListEditor {
    pub kind: StringListKind,
    /// Working copy of the list, one entry per configured file or command.
    pub items: Vec<String>,
    /// Cursor over the rows: `0..items.len()`, then add, then done.
    pub selected: usize,
    /// Buffer while typing an entry; `None` while navigating.
    pub input: Option<TextInput>,
    /// Which entry `input` is rewriting, or `None` when it is a new one.
    pub editing_index: Option<usize>,
}

impl StringListEditor {
    /// Opens the editor on a copy of `items`, with the first row selected.
    pub fn new(kind: StringListKind, items: Vec<String>) -> StringListEditor {
        StringListEditor {
            kind,
            items,
            selected: 0,
            input: None,
            editing_index: None,
        }
    }

    /// Index of the "add" row, just past the items.
    pub fn add_row(&self) -> usize {
        self.items.len()
    }

    /// Index of the "done" row, the last one.
    pub fn done_row(&self) -> usize {
        self.items.len() + 1
    }

    /// Total selectable rows: the items plus add and done.
    pub fn rows(&self) -> usize {
        self.items.len() + 2
    }

    /// Starts typing a new entry, appended when committed.
    fn start_add(&mut self) {
        self.selected = self.add_row();
        self.editing_index = None;
        self.input = Some(TextInput::default());
    }

    /// Starts rewriting the entry at `index`.
    fn start_edit(&mut self, index: usize) {
        self.editing_index = Some(index);
        self.input = Some(TextInput::with_value(&self.items[index]));
    }

    /// Drops the entry at `index`, keeping the cursor on a valid row.
    fn remove(&mut self, index: usize) {
        self.items.remove(index);
        self.selected = self.selected.min(self.rows() - 1);
    }

    /// Handles one key press, whether typing an entry or navigating rows.
    fn on_key(&mut self, key: KeyEvent) -> ListOutcome {
        if let Some(mut input) = self.input.take() {
            match key.code {
                KeyCode::Esc => self.editing_index = None,
                KeyCode::Enter => {
                    let value = input.trimmed();
                    match (self.editing_index.take(), value.is_empty()) {
                        (Some(index), false) => self.items[index] = value,
                        (None, false) => {
                            self.items.push(value);
                            self.selected = self.items.len() - 1;
                        }
                        _ => {}
                    }
                }
                _ => {
                    input.on_key(key);
                    self.input = Some(input);
                }
            }
            return ListOutcome::Continue;
        }
        match key.code {
            KeyCode::Esc => return ListOutcome::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows() - 1)
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter if self.selected == self.done_row() => return ListOutcome::Done,
            KeyCode::Enter if self.selected == self.add_row() => self.start_add(),
            KeyCode::Enter => self.start_edit(self.selected),
            KeyCode::Char('a') => self.start_add(),
            KeyCode::Char('d') | KeyCode::Delete if self.selected < self.items.len() => {
                self.remove(self.selected)
            }
            _ => {}
        }
        ListOutcome::Continue
    }
}

/// What a key press did, for the app to act on.
pub enum EditorOutcome {
    Continue,
    /// The file was written; carries its path for the status message.
    Saved(PathBuf),
    /// The user asked for an update check right now.
    CheckForUpdates,
    Cancel,
}

impl ConfigEditor {
    /// Loads the repo's current settings into the editor.
    pub fn load(repo_root: PathBuf) -> anyhow::Result<ConfigEditor> {
        let global_config = config::global_config_path();
        let fields = settings::repo_config_fields(&repo_root, global_config.as_deref())?;
        Ok(ConfigEditor {
            repo_root,
            global_config,
            fields,
            selected: 0,
            editing: None,
            open_list: None,
            string_list: None,
        })
    }

    /// Re-reads every setting from disk, keeping the config-file paths resolved
    /// at load and resetting the cursor. Used each time the Settings tab is
    /// entered, so it never shows values that have gone stale.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        self.fields = settings::repo_config_fields(&self.repo_root, self.global_config.as_deref())?;
        self.selected = 0;
        self.editing = None;
        self.open_list = None;
        self.string_list = None;
        Ok(())
    }

    /// An editor with every field unset, for a repo whose config can't be read
    /// yet (no `.wtm.toml` before setup runs).
    pub fn empty(repo_root: PathBuf) -> ConfigEditor {
        ConfigEditor {
            repo_root,
            global_config: config::global_config_path(),
            fields: RepoConfigFields::default(),
            selected: 0,
            editing: None,
            open_list: None,
            string_list: None,
        }
    }

    /// Current text of a single-value setting row. List rows
    /// ([`OPEN_COMMAND_ROW`], [`COPY_ROW`], [`RUN_ROW`]) read as empty here;
    /// the renderer uses the matching summary method instead.
    pub fn field(&self, row: usize) -> &str {
        match row {
            0 => &self.fields.worktree_dir,
            OPEN_COMMAND_ROW | COPY_ROW | RUN_ROW => "",
            UPDATE_ROW => &self.fields.auto_update_check,
            THEME_ROW => &self.fields.diff_theme,
            LAYOUT_ROW => &self.fields.worktrees_layout,
            BRANCHES_REFRESH_ROW => &self.fields.branches_refresh_mins,
            DIFF_LINE_NUMBERS_ROW => &self.fields.diff_line_numbers,
            _ => "",
        }
    }

    /// One-line summary of a string list: the item itself when there is one,
    /// otherwise a count followed by the items. Empty when nothing is
    /// configured, so the row falls back to "(default)".
    fn items_summary(items: &[String], plural: &str) -> String {
        match items {
            [] => String::new(),
            [one] => one.clone(),
            many => format!("{} {}: {}", many.len(), plural, many.join(" · ")),
        }
    }

    /// The `open_command` row's value as one line.
    pub fn open_command_summary(&self) -> String {
        let commands: Vec<String> = self
            .fields
            .open_command
            .iter()
            .map(|c| c.command.clone())
            .collect();
        Self::items_summary(&commands, "commands")
    }

    /// The `setup.copy` row's value as one line.
    pub fn copy_summary(&self) -> String {
        Self::items_summary(&self.fields.copy, "files")
    }

    /// The `setup.run` row's value as one line.
    pub fn run_summary(&self) -> String {
        Self::items_summary(&self.fields.run, "commands")
    }

    /// Whether a text buffer is open, so the app knows to route printable keys
    /// here instead of treating them as shortcuts.
    pub fn is_typing(&self) -> bool {
        self.editing.is_some()
            || self.open_list.as_ref().is_some_and(|l| l.input.is_some())
            || self.string_list.as_ref().is_some_and(|l| l.input.is_some())
    }

    /// Whether a dialog owns the keyboard here: a field being edited, or a
    /// list editor. The app asks so Tab can't switch tabs out from under it.
    pub fn dialog_open(&self) -> bool {
        self.editing.is_some() || self.list_open()
    }

    /// Whether a list-editor modal is covering the form (so clicks and footer
    /// hints should follow the list, not the settings rows).
    pub fn list_open(&self) -> bool {
        self.open_list.is_some() || self.string_list.is_some()
    }

    fn set_field(&mut self, row: usize, value: String) {
        match row {
            0 => self.fields.worktree_dir = value,
            UPDATE_ROW => self.fields.auto_update_check = value,
            THEME_ROW => self.fields.diff_theme = value,
            LAYOUT_ROW => self.fields.worktrees_layout = value,
            BRANCHES_REFRESH_ROW => self.fields.branches_refresh_mins = value,
            DIFF_LINE_NUMBERS_ROW => self.fields.diff_line_numbers = value,
            // List rows are edited as lists, never as one text value.
            _ => {}
        }
    }

    /// Cycles the update-check toggle through default → on → off, so the
    /// inherited default stays reachable rather than being a one-way door.
    fn cycle_auto_update_check(&mut self) {
        self.fields.auto_update_check = match self.fields.auto_update_check.as_str() {
            "true" => "false".to_string(),
            "false" => String::new(),
            _ => "true".to_string(),
        };
    }

    /// Cycles the diff line-number gutter through default → on → off, so the
    /// inherited default (on) stays reachable rather than being a one-way door.
    fn cycle_diff_line_numbers(&mut self) {
        self.fields.diff_line_numbers = match self.fields.diff_line_numbers.as_str() {
            "true" => "false".to_string(),
            "false" => String::new(),
            _ => "true".to_string(),
        };
    }

    /// Cycles the diff theme through the catalog, then back to the default
    /// (empty string) so the inherited default stays reachable.
    fn cycle_diff_theme(&mut self) {
        let current = self.fields.diff_theme.as_str();
        if current.is_empty() {
            self.fields.diff_theme = DIFF_THEMES[0].0.to_string();
            return;
        }
        let idx = DIFF_THEMES
            .iter()
            .position(|(id, _, _)| *id == current)
            .unwrap_or(0);
        if idx + 1 >= DIFF_THEMES.len() {
            self.fields.diff_theme.clear();
        } else {
            self.fields.diff_theme = DIFF_THEMES[idx + 1].0.to_string();
        }
    }

    /// Cycles the Worktrees-tab layout through the catalog, then back to the
    /// default (empty string) so the inherited default stays reachable.
    fn cycle_worktrees_layout(&mut self) {
        let layouts = config::WORKTREES_LAYOUTS;
        let current = self.fields.worktrees_layout.as_str();
        if current.is_empty() {
            self.fields.worktrees_layout = layouts[0].0.to_string();
            return;
        }
        let idx = layouts
            .iter()
            .position(|(id, _)| *id == current)
            .unwrap_or(0);
        if idx + 1 >= layouts.len() {
            self.fields.worktrees_layout.clear();
        } else {
            self.fields.worktrees_layout = layouts[idx + 1].0.to_string();
        }
    }

    /// Writes every field to disk and applies the chosen diff theme. Called
    /// after each edit so Settings never holds unsaved changes.
    fn save_fields(&mut self, message: &mut Option<String>) -> EditorOutcome {
        match settings::save_config_edits(
            &self.repo_root,
            self.global_config.as_deref(),
            &self.fields,
        ) {
            Ok(path) => {
                // Apply the theme immediately so the next diff draw uses it
                // without requiring a restart.
                let theme = if self.fields.diff_theme.is_empty() {
                    config::DEFAULT_DIFF_THEME
                } else {
                    self.fields.diff_theme.as_str()
                };
                highlight::set_theme(theme);
                EditorOutcome::Saved(path)
            }
            Err(e) => {
                *message = Some(format!("error: {e:#}"));
                EditorOutcome::Continue
            }
        }
    }

    /// Handles one key press. Save errors land in `message` and keep the
    /// editor open.
    pub fn on_key(&mut self, key: KeyEvent, message: &mut Option<String>) -> EditorOutcome {
        // The open_command list editor is modal over the rest of the form, so
        // it sees every key until it is confirmed (list copied back) or
        // cancelled (working copy dropped).
        if let Some(mut list) = self.open_list.take() {
            match list.on_key(key) {
                ListOutcome::Continue => {
                    self.open_list = Some(list);
                    return EditorOutcome::Continue;
                }
                // Persist immediately: leaving Settings reloads from disk, so a
                // list that only lived in memory would vanish on the next visit.
                ListOutcome::Done => {
                    self.fields.open_command = list.commands;
                    return self.save_fields(message);
                }
                ListOutcome::Cancel => return EditorOutcome::Continue,
            }
        }
        if let Some(mut list) = self.string_list.take() {
            match list.on_key(key) {
                ListOutcome::Continue => {
                    self.string_list = Some(list);
                    return EditorOutcome::Continue;
                }
                ListOutcome::Done => {
                    match list.kind {
                        StringListKind::Copy => self.fields.copy = list.items,
                        StringListKind::Run => self.fields.run = list.items,
                    }
                    return self.save_fields(message);
                }
                ListOutcome::Cancel => return EditorOutcome::Continue,
            }
        }
        // While editing, work on the buffer taken out of `self`; Esc and Enter
        // leave it out (cancel / commit+save), other keys drive the text input
        // and put the edited buffer back.
        if let Some(mut input) = self.editing.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    self.set_field(self.selected, input.trimmed());
                    return self.save_fields(message);
                }
                _ => {
                    input.on_key(key);
                    self.editing = Some(input);
                }
            }
            return EditorOutcome::Continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return EditorOutcome::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(ROWS - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Enter if self.selected == CHECK_ROW => {
                return EditorOutcome::CheckForUpdates;
            }
            // The toggles have no free text to type, so Enter and Space both
            // flip them and write immediately.
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == UPDATE_ROW => {
                self.cycle_auto_update_check();
                return self.save_fields(message);
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == THEME_ROW => {
                self.cycle_diff_theme();
                return self.save_fields(message);
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == DIFF_LINE_NUMBERS_ROW => {
                self.cycle_diff_line_numbers();
                return self.save_fields(message);
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == LAYOUT_ROW => {
                self.cycle_worktrees_layout();
                return self.save_fields(message);
            }
            // List rows open their own editor rather than a text input, so an
            // entry containing a comma stays one item.
            KeyCode::Enter if self.selected == OPEN_COMMAND_ROW => {
                self.open_list = Some(OpenCommandEditor::new(self.fields.open_command.clone()))
            }
            KeyCode::Enter if self.selected == COPY_ROW => {
                self.string_list = Some(StringListEditor::new(
                    StringListKind::Copy,
                    self.fields.copy.clone(),
                ))
            }
            KeyCode::Enter if self.selected == RUN_ROW => {
                self.string_list = Some(StringListEditor::new(
                    StringListKind::Run,
                    self.fields.run.clone(),
                ))
            }
            KeyCode::Enter
                if self.selected < TEXT_ROWS || self.selected == BRANCHES_REFRESH_ROW =>
            {
                // Prefill with the effective default when the field is unset so
                // the user sees what they are changing from.
                let initial = if self.selected == BRANCHES_REFRESH_ROW
                    && self.fields.branches_refresh_mins.is_empty()
                {
                    config::DEFAULT_BRANCHES_REFRESH_MINS.to_string()
                } else {
                    self.field(self.selected).to_string()
                };
                self.editing = Some(TextInput::with_value(&initial))
            }
            _ => {}
        }
        EditorOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn editor() -> ConfigEditor {
        ConfigEditor {
            repo_root: PathBuf::from("/nonexistent"),
            global_config: None,
            fields: RepoConfigFields {
                worktree_dir: "sibling".to_string(),
                ..RepoConfigFields::default()
            },
            selected: 0,
            editing: None,
            open_list: None,
            string_list: None,
        }
    }

    /// Just the templates of a command list, so an assertion can name the
    /// commands without spelling out each entry's mode and scope.
    fn texts(commands: &[OpenCommand]) -> Vec<String> {
        commands.iter().map(|c| c.command.clone()).collect()
    }

    fn press(ed: &mut ConfigEditor, code: KeyCode) -> EditorOutcome {
        let mut msg = None;
        ed.on_key(KeyEvent::from(code), &mut msg)
    }

    #[test]
    fn editing_a_field_supports_mid_string_cursor_edits() {
        // The cursor-aware buffer allows Home + mid-string insert, which the
        // old append-only String could not do.
        let mut ed = editor();
        press(&mut ed, KeyCode::Enter); // edit worktree_dir ("sibling")
        press(&mut ed, KeyCode::Home); // jump to the start
        press(&mut ed, KeyCode::Char('~'));
        press(&mut ed, KeyCode::Char('/'));
        press(&mut ed, KeyCode::Enter); // commit
        assert_eq!(ed.fields.worktree_dir, "~/sibling");
        assert!(ed.editing.is_none());
    }

    #[test]
    fn update_check_row_cycles_instead_of_opening_an_input() {
        let mut ed = editor();
        ed.selected = UPDATE_ROW;
        press(&mut ed, KeyCode::Enter);
        assert_eq!(ed.fields.auto_update_check, "true");
        assert!(
            ed.editing.is_none(),
            "the toggle must not open a text input"
        );
        press(&mut ed, KeyCode::Char(' '));
        assert_eq!(ed.fields.auto_update_check, "false");
        press(&mut ed, KeyCode::Enter);
        assert_eq!(
            ed.fields.auto_update_check, "",
            "third press returns to the inherited default"
        );
    }

    #[test]
    fn theme_row_cycles_through_catalog_then_default() {
        let mut ed = editor();
        ed.selected = THEME_ROW;
        press(&mut ed, KeyCode::Enter);
        assert_eq!(ed.fields.diff_theme, DIFF_THEMES[0].0);
        for _ in 0..DIFF_THEMES.len() - 1 {
            press(&mut ed, KeyCode::Enter);
        }
        assert_eq!(ed.fields.diff_theme, DIFF_THEMES[DIFF_THEMES.len() - 1].0);
        press(&mut ed, KeyCode::Enter);
        assert_eq!(ed.fields.diff_theme, "", "wraps back to the default");
    }

    #[test]
    fn layout_row_cycles_through_layouts_then_default() {
        let mut ed = editor();
        ed.selected = LAYOUT_ROW;
        press(&mut ed, KeyCode::Enter);
        assert_eq!(ed.fields.worktrees_layout, "two_panel");
        assert!(
            ed.editing.is_none(),
            "the layout row must not open a text input"
        );
        press(&mut ed, KeyCode::Char(' '));
        assert_eq!(ed.fields.worktrees_layout, "three_panel");
        press(&mut ed, KeyCode::Enter);
        assert_eq!(
            ed.fields.worktrees_layout, "",
            "the inherited default stays reachable"
        );
    }

    /// Types `text` into whatever input is open, one key at a time.
    fn type_str(ed: &mut ConfigEditor, text: &str) {
        for ch in text.chars() {
            press(ed, KeyCode::Char(ch));
        }
    }

    /// The editor sitting on the open_command row with `commands` configured.
    fn list_editor(commands: &[&str]) -> ConfigEditor {
        let mut ed = editor();
        ed.fields.open_command = commands.iter().map(|c| OpenCommand::new(*c)).collect();
        ed.selected = OPEN_COMMAND_ROW;
        ed
    }

    /// List editor backed by a real config file, so `[ done ]` can persist.
    fn list_editor_on_disk(commands: &[&str]) -> (tempfile::TempDir, ConfigEditor) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(config::CONFIG_FILE), "").unwrap();
        let fields = RepoConfigFields {
            open_command: commands.iter().map(|c| OpenCommand::new(*c)).collect(),
            ..RepoConfigFields::default()
        };
        settings::save_config_edits(dir.path(), None, &fields).unwrap();
        let ed = ConfigEditor {
            repo_root: dir.path().to_path_buf(),
            global_config: None,
            fields,
            selected: OPEN_COMMAND_ROW,
            editing: None,
            open_list: None,
            string_list: None,
        };
        (dir, ed)
    }

    /// Moves to `[ done ]` and confirms, returning the outcome.
    fn finish_list(ed: &mut ConfigEditor) -> EditorOutcome {
        let list = ed.open_list.as_ref().expect("list editor open");
        for _ in list.selected..list.done_row() {
            press(ed, KeyCode::Down);
        }
        press(ed, KeyCode::Enter)
    }

    #[test]
    fn open_command_row_opens_the_list_editor_not_a_text_input() {
        let mut ed = list_editor(&["cursor {path}"]);
        press(&mut ed, KeyCode::Enter);
        assert!(ed.editing.is_none(), "the list row has no single value");
        let list = ed.open_list.as_ref().expect("list editor open");
        assert_eq!(texts(&list.commands), ["cursor {path}"]);
        assert_eq!(list.add_row(), 1);
        assert_eq!(list.done_row(), 2);
    }

    #[test]
    fn open_command_list_adds_a_command() {
        let (_dir, mut ed) = list_editor_on_disk(&["cursor {path}"]);
        press(&mut ed, KeyCode::Enter); // open the list editor
        press(&mut ed, KeyCode::Char('a')); // start a new entry
        type_str(&mut ed, "open {path}");
        press(&mut ed, KeyCode::Enter); // commit the entry
        let outcome = finish_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert!(ed.open_list.is_none());
        assert_eq!(
            texts(&ed.fields.open_command),
            ["cursor {path}", "open {path}"]
        );
    }

    #[test]
    fn open_command_list_edits_an_existing_command() {
        let (_dir, mut ed) = list_editor_on_disk(&["cursor {path}", "open {path}"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Down); // select the second command
        press(&mut ed, KeyCode::Enter); // edit it
        press(&mut ed, KeyCode::End);
        type_str(&mut ed, " -a Finder");
        press(&mut ed, KeyCode::Enter); // commit the entry
        let outcome = finish_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert_eq!(
            texts(&ed.fields.open_command),
            ["cursor {path}", "open {path} -a Finder"]
        );
    }

    #[test]
    fn open_command_list_removes_a_command() {
        let (_dir, mut ed) = list_editor_on_disk(&["cursor {path}", "open {path}"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('d')); // remove the first command
        let outcome = finish_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert_eq!(texts(&ed.fields.open_command), ["open {path}"]);
    }

    #[test]
    fn open_command_list_keeps_commas_inside_one_entry() {
        let (_dir, mut ed) = list_editor_on_disk(&[]);
        press(&mut ed, KeyCode::Enter); // open on the (empty) list
        press(&mut ed, KeyCode::Enter); // the add row is row 0 when empty
        type_str(&mut ed, "sh -c 'cd {path}, npm start'");
        press(&mut ed, KeyCode::Enter);
        let outcome = finish_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert_eq!(
            texts(&ed.fields.open_command),
            ["sh -c 'cd {path}, npm start'"],
            "a comma inside a command must not split it"
        );
    }

    #[test]
    fn escaping_the_list_editor_discards_its_edits() {
        let mut ed = list_editor(&["cursor {path}"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('d')); // remove the only command
        press(&mut ed, KeyCode::Char('a')); // and start another
        type_str(&mut ed, "open {path}");
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Esc); // discard the whole session
        assert!(ed.open_list.is_none());
        assert_eq!(
            texts(&ed.fields.open_command),
            ["cursor {path}"],
            "Esc leaves the saved list untouched"
        );
    }

    #[test]
    fn escaping_an_entry_leaves_the_list_open_and_unchanged() {
        let mut ed = list_editor(&["cursor {path}"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Enter); // edit the first command
        type_str(&mut ed, "xx");
        press(&mut ed, KeyCode::Esc); // abandon just this entry
        let list = ed.open_list.as_ref().expect("still open");
        assert!(list.input.is_none());
        assert_eq!(texts(&list.commands), ["cursor {path}"]);
    }

    #[test]
    fn a_blank_entry_adds_nothing() {
        let mut ed = list_editor(&[]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('a'));
        press(&mut ed, KeyCode::Enter); // commit an empty buffer
        let list = ed.open_list.as_ref().expect("still open");
        assert!(list.commands.is_empty());
    }

    #[test]
    fn open_command_summary_counts_multiple_commands() {
        let ed = list_editor(&["cursor {path}"]);
        assert_eq!(ed.open_command_summary(), "cursor {path}");
        let ed = list_editor(&["cursor {path}", "open {path}"]);
        assert_eq!(
            ed.open_command_summary(),
            "2 commands: cursor {path} · open {path}"
        );
        assert_eq!(list_editor(&[]).open_command_summary(), "");
    }

    /// The editor sitting on a setup.copy or setup.run row with `items`.
    fn string_list_editor(kind: StringListKind, items: &[&str]) -> ConfigEditor {
        let mut ed = editor();
        let owned: Vec<String> = items.iter().map(|s| (*s).to_string()).collect();
        match kind {
            StringListKind::Copy => {
                ed.fields.copy = owned;
                ed.selected = COPY_ROW;
            }
            StringListKind::Run => {
                ed.fields.run = owned;
                ed.selected = RUN_ROW;
            }
        }
        ed
    }

    /// String-list editor backed by a real config file, so `[ done ]` can persist.
    fn string_list_editor_on_disk(
        kind: StringListKind,
        items: &[&str],
    ) -> (tempfile::TempDir, ConfigEditor) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(config::CONFIG_FILE), "").unwrap();
        let owned: Vec<String> = items.iter().map(|s| (*s).to_string()).collect();
        let mut fields = RepoConfigFields::default();
        match kind {
            StringListKind::Copy => fields.copy = owned.clone(),
            StringListKind::Run => fields.run = owned.clone(),
        }
        settings::save_config_edits(dir.path(), None, &fields).unwrap();
        let selected = match kind {
            StringListKind::Copy => COPY_ROW,
            StringListKind::Run => RUN_ROW,
        };
        let ed = ConfigEditor {
            repo_root: dir.path().to_path_buf(),
            global_config: None,
            fields,
            selected,
            editing: None,
            open_list: None,
            string_list: None,
        };
        (dir, ed)
    }

    /// Moves to `[ done ]` in the string-list editor and confirms.
    fn finish_string_list(ed: &mut ConfigEditor) -> EditorOutcome {
        let list = ed.string_list.as_ref().expect("string list editor open");
        for _ in list.selected..list.done_row() {
            press(ed, KeyCode::Down);
        }
        press(ed, KeyCode::Enter)
    }

    #[test]
    fn setup_run_row_opens_the_list_editor_not_a_text_input() {
        let mut ed = string_list_editor(StringListKind::Run, &["npm install"]);
        press(&mut ed, KeyCode::Enter);
        assert!(ed.editing.is_none(), "the list row has no single value");
        let list = ed.string_list.as_ref().expect("list editor open");
        assert_eq!(list.kind, StringListKind::Run);
        assert_eq!(list.items, ["npm install"]);
        assert_eq!(list.add_row(), 1);
        assert_eq!(list.done_row(), 2);
    }

    #[test]
    fn setup_copy_row_opens_the_list_editor_not_a_text_input() {
        let mut ed = string_list_editor(StringListKind::Copy, &[".env"]);
        press(&mut ed, KeyCode::Enter);
        assert!(ed.editing.is_none());
        let list = ed.string_list.as_ref().expect("list editor open");
        assert_eq!(list.kind, StringListKind::Copy);
        assert_eq!(list.items, [".env"]);
    }

    #[test]
    fn setup_run_list_adds_edits_and_keeps_commas_whole() {
        let (_dir, mut ed) = string_list_editor_on_disk(StringListKind::Run, &["npm install"]);
        press(&mut ed, KeyCode::Enter); // open
        press(&mut ed, KeyCode::Enter); // edit "npm install"
        press(&mut ed, KeyCode::End);
        type_str(&mut ed, " --legacy-peer-deps");
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('a'));
        type_str(&mut ed, "sh -c 'echo hi, there'");
        press(&mut ed, KeyCode::Enter);
        let outcome = finish_string_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert!(ed.string_list.is_none());
        assert_eq!(
            ed.fields.run,
            [
                "npm install --legacy-peer-deps".to_string(),
                "sh -c 'echo hi, there'".to_string()
            ]
        );
    }

    #[test]
    fn setup_copy_list_adds_a_file() {
        let (_dir, mut ed) = string_list_editor_on_disk(StringListKind::Copy, &[".env"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('a'));
        type_str(&mut ed, ".env.local");
        press(&mut ed, KeyCode::Enter);
        let outcome = finish_string_list(&mut ed);
        assert!(matches!(outcome, EditorOutcome::Saved(_)));
        assert_eq!(ed.fields.copy, [".env", ".env.local"]);
    }

    #[test]
    fn escaping_the_setup_list_editor_discards_its_edits() {
        let mut ed = string_list_editor(StringListKind::Run, &["npm install"]);
        press(&mut ed, KeyCode::Enter);
        press(&mut ed, KeyCode::Char('d'));
        press(&mut ed, KeyCode::Esc);
        assert!(ed.string_list.is_none());
        assert_eq!(ed.fields.run, ["npm install"]);
    }

    #[test]
    fn setup_summaries_count_multiple_items() {
        let ed = string_list_editor(StringListKind::Copy, &[".env"]);
        assert_eq!(ed.copy_summary(), ".env");
        let ed = string_list_editor(StringListKind::Copy, &[".env", ".env.local"]);
        assert_eq!(ed.copy_summary(), "2 files: .env · .env.local");
        assert_eq!(
            string_list_editor(StringListKind::Copy, &[]).copy_summary(),
            ""
        );
        let ed = string_list_editor(StringListKind::Run, &["npm ci", "npm run build"]);
        assert_eq!(ed.run_summary(), "2 commands: npm ci · npm run build");
    }

    #[test]
    fn check_row_is_past_the_fields() {
        assert_eq!(CHECK_ROW, FIELD_ROWS);
        assert_eq!(row_at_line(check_line()), Some(CHECK_ROW));
        assert_eq!(row_at_line(preview_line()), None);
        assert_eq!(row_at_line(theme_preview_label_line()), None);
        assert_eq!(row_at_line(theme_preview_line()), None);
        assert_eq!(row_at_line(line_of_row(LAYOUT_ROW)), Some(LAYOUT_ROW));
        assert_eq!(
            row_at_line(line_of_row(LAYOUT_ROW) + 1),
            None,
            "layout description is not selectable"
        );
        assert_eq!(
            row_at_line(line_of_row(BRANCHES_REFRESH_ROW)),
            Some(BRANCHES_REFRESH_ROW)
        );
        assert_eq!(
            row_at_line(line_of_row(BRANCHES_REFRESH_ROW) + 1),
            None,
            "branches refresh description is not selectable"
        );
        // Section headers and blanks are not selectable.
        assert_eq!(row_at_line(0), None, "repo section header");
        assert_eq!(row_at_line(1), None, "blank under repo section");
        // Theme sample sits under the theme row and before the layout row.
        assert!(theme_preview_label_line() > line_of_row(THEME_ROW));
        assert!(theme_preview_line() + THEME_PREVIEW_SAMPLE_LINES - 1 < line_of_row(LAYOUT_ROW));
        assert_eq!(
            THEME_PREVIEW_SAMPLE_LINES,
            highlight::THEME_PREVIEW_SAMPLE.lines().count(),
            "sample line count must match the rendered snippet"
        );
        // Each ordinary setting leaves a blank before the next value line.
        assert_eq!(
            line_of_row(1) - line_of_row(0),
            SETTING_LINES,
            "settings are spaced with a blank separator"
        );
    }
}
