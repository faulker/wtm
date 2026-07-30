//! In-app editor for the repo's `.wtm.toml`, backing the Settings tab so
//! settings can be changed without editing the file by hand.
//!
//! It shows the repo-level settings as editable rows, the update-check toggle,
//! the diff-theme cycle (with a live colour sample), a save row, and a
//! "check for updates now" row. Saving preserves comments and only writes the
//! keys the repo actually sets; a cleared field unsets that key so the default
//! (or global value) applies again.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::TextInput;
use super::highlight::{self, DIFF_THEMES};
use crate::config;
use crate::settings::{self, RepoConfigFields};

/// Rows holding a free-text value (worktree_dir, open_command, setup.copy,
/// setup.run). Enter on one of these opens the text input.
pub const TEXT_ROWS: usize = 4;
/// Index of the update-check toggle, which Enter cycles rather than edits.
pub const UPDATE_ROW: usize = TEXT_ROWS;
/// Index of the diff-theme cycle row.
pub const THEME_ROW: usize = TEXT_ROWS + 1;
/// Number of setting rows, text fields plus the two cycle toggles.
pub const FIELD_ROWS: usize = TEXT_ROWS + 2;
/// Index of the save row.
pub const SAVE_ROW: usize = FIELD_ROWS;
/// Index of the "check for updates now" row.
pub const CHECK_ROW: usize = FIELD_ROWS + 1;
/// Total selectable rows.
pub const ROWS: usize = FIELD_ROWS + 2;

// Line offsets within the Settings tab's rendered form, so the renderer and
// the click handler cannot drift apart. Each setting row draws a value line
// followed by a dim hint line, filling `FIELD_ROWS * 2` lines. After that:
// the worktree-location preview, a labelled theme-colour sample, the version
// line, then the two action rows.
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
pub const SAVE_LINE: usize = VERSION_LINE + 1;
pub const CHECK_LINE: usize = SAVE_LINE + 1;
/// Total lines the form occupies.
pub const FORM_LINES: usize = CHECK_LINE + 1;

/// The row a click on form line `line` selects, or `None` for the hint,
/// preview, theme sample, and version lines, which are not selectable.
pub fn row_at_line(line: usize) -> Option<usize> {
    match line {
        SAVE_LINE => Some(SAVE_ROW),
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
    /// The global config file `auto_update_check` / `diff_theme` are read from
    /// and written to, resolved once at load so a save cannot land somewhere
    /// else. `None` on a system with no locatable global config.
    pub global_config: Option<PathBuf>,
    /// The setting values as shown, each empty when unset.
    pub fields: RepoConfigFields,
    /// Selected row: 0..FIELD_ROWS edit a setting, then save, then check-now.
    pub selected: usize,
    /// Cursor-aware buffer while editing the selected row; `None` when
    /// navigating. Shares `TextInput` with the other prompts so `←/→`,
    /// Home/End, and mid-string edits work here too.
    pub editing: Option<TextInput>,
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
        })
    }

    /// Re-reads every setting from disk, keeping the config-file paths resolved
    /// at load and resetting the cursor. Used each time the Settings tab is
    /// entered, so it never shows values that have gone stale.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        self.fields = settings::repo_config_fields(&self.repo_root, self.global_config.as_deref())?;
        self.selected = 0;
        self.editing = None;
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
        }
    }

    /// Current text of a setting row.
    pub fn field(&self, row: usize) -> &str {
        match row {
            0 => &self.fields.worktree_dir,
            1 => &self.fields.open_command,
            2 => &self.fields.copy,
            3 => &self.fields.run,
            UPDATE_ROW => &self.fields.auto_update_check,
            _ => &self.fields.diff_theme,
        }
    }

    fn set_field(&mut self, row: usize, value: String) {
        match row {
            0 => self.fields.worktree_dir = value,
            1 => self.fields.open_command = value,
            2 => self.fields.copy = value,
            3 => self.fields.run = value,
            UPDATE_ROW => self.fields.auto_update_check = value,
            _ => self.fields.diff_theme = value,
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

    /// Handles one key press. Save errors land in `message` and keep the
    /// editor open.
    pub fn on_key(&mut self, key: KeyEvent, message: &mut Option<String>) -> EditorOutcome {
        // While editing, work on the buffer taken out of `self`; Esc and Enter
        // leave it out (cancel / commit), other keys drive the text input and
        // put the edited buffer back.
        if let Some(mut input) = self.editing.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.set_field(self.selected, input.trimmed()),
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
            KeyCode::Enter if self.selected == SAVE_ROW => {
                match settings::save_config_edits(
                    &self.repo_root,
                    self.global_config.as_deref(),
                    &self.fields,
                ) {
                    Ok(path) => {
                        // Apply the theme immediately so the next diff draw
                        // uses it without requiring a restart.
                        let theme = if self.fields.diff_theme.is_empty() {
                            config::DEFAULT_DIFF_THEME
                        } else {
                            self.fields.diff_theme.as_str()
                        };
                        highlight::set_theme(theme);
                        return EditorOutcome::Saved(path);
                    }
                    Err(e) => *message = Some(format!("error: {e:#}")),
                }
            }
            // The toggles have no free text to type, so Enter and Space both
            // flip them instead of opening an input.
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == UPDATE_ROW => {
                self.cycle_auto_update_check()
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.selected == THEME_ROW => {
                self.cycle_diff_theme()
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
    fn save_and_check_rows_are_past_the_fields() {
        assert_eq!(SAVE_ROW, FIELD_ROWS);
        assert_eq!(CHECK_ROW, FIELD_ROWS + 1);
        assert_eq!(row_at_line(SAVE_LINE), Some(SAVE_ROW));
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
