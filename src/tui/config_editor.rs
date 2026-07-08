//! In-app editor for the repo's `.wtm.toml`, reachable from the worktree list
//! so settings can be changed without editing the file by hand.
//!
//! It shows the repo-level settings as editable rows plus a save row.
//! Saving preserves comments and only writes the keys the repo actually sets;
//! a cleared field unsets that key so the default (or global value) applies.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::settings;

/// Number of editable setting rows (worktree_dir, open_command, setup.copy,
/// setup.run).
pub const FIELD_ROWS: usize = 4;
/// Total selectable rows, including the trailing save row.
pub const ROWS: usize = FIELD_ROWS + 1;

/// State of the config editor overlay.
pub struct ConfigEditor {
    pub repo_root: PathBuf,
    /// Repo-level `worktree_dir` (empty means unset).
    pub worktree_dir: String,
    /// Repo-level `open_command` (empty means unset).
    pub open_command: String,
    /// Comma-joined `setup.copy`.
    pub copy: String,
    /// Comma-joined `setup.run`.
    pub run: String,
    /// Selected row: 0..FIELD_ROWS edit a setting, ROWS-1 is save.
    pub selected: usize,
    /// Text buffer while editing the selected row; `None` when navigating.
    pub editing: Option<String>,
}

/// What a key press did, for the app to act on.
pub enum EditorOutcome {
    Continue,
    /// The file was written; carries its path for the status message.
    Saved(PathBuf),
    Cancel,
}

impl ConfigEditor {
    /// Loads the repo's current settings into the editor.
    pub fn load(repo_root: PathBuf) -> anyhow::Result<ConfigEditor> {
        let fields = settings::repo_config_fields(&repo_root)?;
        Ok(ConfigEditor {
            repo_root,
            worktree_dir: fields.worktree_dir,
            open_command: fields.open_command,
            copy: fields.copy,
            run: fields.run,
            selected: 0,
            editing: None,
        })
    }

    /// Current text of an editable row.
    pub fn field(&self, row: usize) -> &str {
        match row {
            0 => &self.worktree_dir,
            1 => &self.open_command,
            2 => &self.copy,
            _ => &self.run,
        }
    }

    fn set_field(&mut self, row: usize, value: String) {
        match row {
            0 => self.worktree_dir = value,
            1 => self.open_command = value,
            2 => self.copy = value,
            _ => self.run = value,
        }
    }

    /// Handles one key press. Save errors land in `message` and keep the
    /// editor open.
    pub fn on_key(&mut self, key: KeyEvent, message: &mut Option<String>) -> EditorOutcome {
        // While editing, work on the buffer taken out of `self`; Esc and Enter
        // leave it out (cancel / commit), other keys put the edited buffer back.
        if let Some(mut buf) = self.editing.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.set_field(self.selected, buf.trim().to_string()),
                KeyCode::Backspace => {
                    buf.pop();
                    self.editing = Some(buf);
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.editing = Some(buf);
                }
                _ => self.editing = Some(buf),
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
            KeyCode::Enter if self.selected == ROWS - 1 => {
                match settings::save_config_edits(
                    &self.repo_root,
                    &self.worktree_dir,
                    &self.open_command,
                    &self.copy,
                    &self.run,
                ) {
                    Ok(path) => return EditorOutcome::Saved(path),
                    Err(e) => *message = Some(format!("error: {e:#}")),
                }
            }
            KeyCode::Enter => self.editing = Some(self.field(self.selected).to_string()),
            _ => {}
        }
        EditorOutcome::Continue
    }
}
