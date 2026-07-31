//! In-app editor for the repo's `.wtm.toml`, backing the Settings tab so
//! settings can be changed without editing the file by hand.
//!
//! It shows the repo-level settings as editable rows, the update-check toggle,
//! the diff-theme cycle (with a live colour sample), the Worktrees-tab layout
//! cycle, and a "check for updates now" row. Every change is written
//! immediately (text fields on Enter, cycle rows on Enter/Space, the
//! open_command list on `[ done ]`), preserving comments and only writing the
//! keys that are set; a cleared field unsets that key so the default (or
//! global value) applies again.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::TextInput;
use super::highlight::{self, DIFF_THEMES};
use crate::config;
use crate::settings::{self, RepoConfigFields};

/// Rows holding an editable value (worktree_dir, open_command, setup.copy,
/// setup.run). Enter on one of these opens the text input, except
/// [`OPEN_COMMAND_ROW`], which opens the list editor.
pub const TEXT_ROWS: usize = 4;
/// Index of the `open_command` row, whose value is a list of commands the
/// Worktrees-tab `o` key can run rather than one string.
pub const OPEN_COMMAND_ROW: usize = 1;
/// Index of the update-check toggle, which Enter cycles rather than edits.
pub const UPDATE_ROW: usize = TEXT_ROWS;
/// Index of the diff-theme cycle row.
pub const THEME_ROW: usize = TEXT_ROWS + 1;
/// Index of the Worktrees-tab layout cycle row.
pub const LAYOUT_ROW: usize = TEXT_ROWS + 2;
/// Number of setting rows, text fields plus the three cycle rows.
pub const FIELD_ROWS: usize = TEXT_ROWS + 3;
/// Index of the "check for updates now" row.
pub const CHECK_ROW: usize = FIELD_ROWS;
/// Total selectable rows.
pub const ROWS: usize = FIELD_ROWS + 1;

// Line offsets within the Settings tab's rendered form, so the renderer and
// the click handler cannot drift apart. Each setting row draws a value line
// followed by a dim hint line, filling `FIELD_ROWS * 2` lines. After that:
// the worktree-location preview, a labelled theme-colour sample, the version
// line, then the check-for-updates action row.
/// Line showing where worktrees will actually be created.
pub const PREVIEW_LINE: usize = FIELD_ROWS * 2;
/// Label line above the theme colour sample ("diff colours look like").
pub const THEME_PREVIEW_LABEL_LINE: usize = PREVIEW_LINE + 1;
/// Number of sample lines drawn under [`THEME_PREVIEW_LABEL_LINE`].
/// Kept in sync with `highlight::THEME_PREVIEW_SAMPLE`'s line count.
pub const THEME_PREVIEW_SAMPLE_LINES: usize = 3;
/// First sample line of the theme colour preview.
pub const THEME_PREVIEW_LINE: usize = THEME_PREVIEW_LABEL_LINE + 1;
/// Line showing the running version and any update found.
pub const VERSION_LINE: usize = THEME_PREVIEW_LINE + THEME_PREVIEW_SAMPLE_LINES;
pub const CHECK_LINE: usize = VERSION_LINE + 1;
/// Total lines the form occupies.
pub const FORM_LINES: usize = CHECK_LINE + 1;

/// The row a click on form line `line` selects, or `None` for the hint,
/// preview, theme sample, and version lines, which are not selectable.
pub fn row_at_line(line: usize) -> Option<usize> {
    match line {
        CHECK_LINE => Some(CHECK_ROW),
        // Only a field's value line (the even offset) selects it; its hint line
        // below is decoration.
        _ if line < FIELD_ROWS * 2 && line.is_multiple_of(2) => Some(line / 2),
        _ => None,
    }
}

/// State of the Settings tab's editor.
pub struct ConfigEditor {
    pub repo_root: PathBuf,
    /// The global config file `auto_update_check`, `diff_theme`, and
    /// `worktrees_layout` are read from and written to, resolved once at load
    /// so a save cannot land somewhere else. `None` on a system with no
    /// locatable global config.
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
}

/// The `open_command` list editor: a modal over the Settings tab for adding,
/// editing, and removing individual commands. It works on a copy of the list
/// and only hands it back when the user confirms.
///
/// Rows are the commands, then an "add" row, then a "done" row, so every
/// action is reachable by arrow keys as well as by its shortcut.
pub struct OpenCommandEditor {
    /// Working copy of the list, one shell template per entry.
    pub commands: Vec<String>,
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
    pub fn new(commands: Vec<String>) -> OpenCommandEditor {
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
        self.input = Some(TextInput::with_value(&self.commands[index]));
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
                        (Some(index), false) => self.commands[index] = value,
                        (None, false) => {
                            self.commands.push(value);
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
        }
    }

    /// Current text of a single-value setting row. [`OPEN_COMMAND_ROW`] holds
    /// a list rather than one value, so it reads as empty here; the renderer
    /// uses [`ConfigEditor::open_command_summary`] for it instead.
    pub fn field(&self, row: usize) -> &str {
        match row {
            0 => &self.fields.worktree_dir,
            OPEN_COMMAND_ROW => "",
            2 => &self.fields.copy,
            3 => &self.fields.run,
            UPDATE_ROW => &self.fields.auto_update_check,
            THEME_ROW => &self.fields.diff_theme,
            _ => &self.fields.worktrees_layout,
        }
    }

    /// The `open_command` row's value as one line: the command itself when
    /// there is one, otherwise a count followed by the commands. Empty when
    /// nothing is configured, so the row falls back to "(default)".
    pub fn open_command_summary(&self) -> String {
        match self.fields.open_command.len() {
            0 => String::new(),
            1 => self.fields.open_command[0].clone(),
            n => format!("{n} commands: {}", self.fields.open_command.join(" · ")),
        }
    }

    /// Whether a text buffer is open, so the app knows to route printable keys
    /// here instead of treating them as shortcuts.
    pub fn is_typing(&self) -> bool {
        self.editing.is_some() || self.open_list.as_ref().is_some_and(|l| l.input.is_some())
    }

    fn set_field(&mut self, row: usize, value: String) {
        match row {
            0 => self.fields.worktree_dir = value,
            2 => self.fields.copy = value,
            3 => self.fields.run = value,
            UPDATE_ROW => self.fields.auto_update_check = value,
            THEME_ROW => self.fields.diff_theme = value,
            LAYOUT_ROW => self.fields.worktrees_layout = value,
            // `open_command` is edited as a list, never as one text value.
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
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == LAYOUT_ROW => {
                self.cycle_worktrees_layout();
                return self.save_fields(message);
            }
            // The list row opens its own editor rather than a text input, so
            // a command containing a comma stays one entry.
            KeyCode::Enter if self.selected == OPEN_COMMAND_ROW => {
                self.open_list = Some(OpenCommandEditor::new(self.fields.open_command.clone()))
            }
            KeyCode::Enter if self.selected < TEXT_ROWS => {
                self.editing = Some(TextInput::with_value(self.field(self.selected)))
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
        }
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
        ed.fields.open_command = commands.iter().map(|c| c.to_string()).collect();
        ed.selected = OPEN_COMMAND_ROW;
        ed
    }

    /// List editor backed by a real config file, so `[ done ]` can persist.
    fn list_editor_on_disk(commands: &[&str]) -> (tempfile::TempDir, ConfigEditor) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(config::CONFIG_FILE), "").unwrap();
        let fields = RepoConfigFields {
            open_command: commands.iter().map(|c| c.to_string()).collect(),
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
        assert_eq!(list.commands, ["cursor {path}"]);
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
        assert_eq!(ed.fields.open_command, ["cursor {path}", "open {path}"]);
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
            ed.fields.open_command,
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
        assert_eq!(ed.fields.open_command, ["open {path}"]);
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
            ed.fields.open_command,
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
            ed.fields.open_command,
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
        assert_eq!(list.commands, ["cursor {path}"]);
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

    #[test]
    fn check_row_is_past_the_fields() {
        assert_eq!(CHECK_ROW, FIELD_ROWS);
        assert_eq!(row_at_line(CHECK_LINE), Some(CHECK_ROW));
        assert_eq!(row_at_line(PREVIEW_LINE), None);
        assert_eq!(row_at_line(THEME_PREVIEW_LABEL_LINE), None);
        assert_eq!(row_at_line(THEME_PREVIEW_LINE), None);
        assert_eq!(
            THEME_PREVIEW_SAMPLE_LINES,
            highlight::THEME_PREVIEW_SAMPLE.lines().count(),
            "sample line count must match the rendered snippet"
        );
    }
}
