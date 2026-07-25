//! First-run setup wizard shown when the repo has no `.wtm.toml` yet.
//!
//! The wizard first offers to clone settings from another repo (typed path or
//! file browser), then walks the same questions as `wtm init`. Both routes end
//! on a review screen where every setting can still be edited before the
//! config file is written.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::app::TextInput;
use crate::config::{self, CONFIG_FILE, DEFAULT_LOCATION, LOCATION_PRESETS};
use crate::settings::{self, ConfigDraft};

/// State of the first-run setup wizard.
pub struct SetupWizard {
    pub repo_root: PathBuf,
    pub step: Step,
    /// Accumulated answers; written as `.wtm.toml` when the wizard finishes.
    pub draft: ConfigDraft,
}

/// Which wizard question is on screen.
pub enum Step {
    /// Yes/no: clone settings from another location?
    CloneAsk { yes: bool },
    /// Typed path to a repo or `.wtm.toml` to clone from.
    ClonePath { input: TextInput },
    /// File browser alternative to typing the path; `prior_input` restores
    /// the typed path when the browser is cancelled.
    CloneBrowse {
        browser: FileBrowser,
        prior_input: TextInput,
    },
    /// Where new worktrees go: the presets plus "somewhere else".
    Location { selected: usize },
    /// Manual path for the "somewhere else" choice.
    LocationCustom { input: TextInput },
    /// Comma-separated files to copy into new worktrees.
    CopyFiles { input: TextInput },
    /// Setup commands, entered one per line until a blank one.
    RunCommands {
        commands: Vec<String>,
        input: TextInput,
    },
    /// Editable summary of the draft; the last row writes the file.
    Review {
        selected: usize,
        editing: Option<TextInput>,
    },
}

/// Rows on the review screen, in order.
pub const REVIEW_ROWS: usize = 4;

impl Step {
    /// A short "where am I" label for the wizard header. The two routes have
    /// different lengths (cloning skips the location/copy/run questions), so the
    /// total adapts to the branch the user took. Esc always steps back one.
    pub fn progress(&self) -> &'static str {
        match self {
            Step::CloneAsk { .. } => "step 1 · start",
            Step::ClonePath { .. } | Step::CloneBrowse { .. } => "step 2 of 3 · clone route",
            Step::Location { .. } | Step::LocationCustom { .. } => "step 2 of 5",
            Step::CopyFiles { .. } => "step 3 of 5",
            Step::RunCommands { .. } => "step 4 of 5",
            Step::Review { .. } => "final step · review & write",
        }
    }
}

/// What a key press did, for the app to act on.
pub enum WizardOutcome {
    Continue,
    /// The draft is final; write it and enter the normal list view.
    Done,
    Quit,
}

impl SetupWizard {
    /// Starts the wizard at the clone question.
    pub fn new(repo_root: PathBuf) -> SetupWizard {
        SetupWizard {
            repo_root,
            // Default to "no": most repos are set up fresh, not cloned.
            step: Step::CloneAsk { yes: false },
            draft: ConfigDraft::default(),
        }
    }

    /// Handles one key press. Errors (bad clone path, unreadable directory)
    /// land in `message` and keep the current step on screen.
    pub fn on_key(&mut self, key: KeyEvent, message: &mut Option<String>) -> WizardOutcome {
        // Take the step by value so transitions can move state between steps.
        let step = std::mem::replace(&mut self.step, Step::CloneAsk { yes: true });
        let (next, outcome) = self.handle(step, key, message);
        self.step = next;
        outcome
    }

    fn handle(
        &mut self,
        step: Step,
        key: KeyEvent,
        message: &mut Option<String>,
    ) -> (Step, WizardOutcome) {
        use WizardOutcome::Continue;
        match step {
            Step::CloneAsk { yes } => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::Char('h')
                | KeyCode::Char('l') => (Step::CloneAsk { yes: !yes }, Continue),
                KeyCode::Char('y') => (
                    Step::ClonePath {
                        input: TextInput::default(),
                    },
                    Continue,
                ),
                KeyCode::Char('n') => (Step::Location { selected: 0 }, Continue),
                KeyCode::Enter if yes => (
                    Step::ClonePath {
                        input: TextInput::default(),
                    },
                    Continue,
                ),
                KeyCode::Enter => (Step::Location { selected: 0 }, Continue),
                KeyCode::Esc | KeyCode::Char('q') => (Step::CloneAsk { yes }, WizardOutcome::Quit),
                _ => (Step::CloneAsk { yes }, Continue),
            },

            Step::ClonePath { mut input } => match key.code {
                KeyCode::Esc => (Step::CloneAsk { yes: false }, Continue),
                KeyCode::Tab => {
                    // Sibling repos are the usual clone source, so start the
                    // browser one level up from this repo.
                    let start = self
                        .repo_root
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.repo_root.clone());
                    match FileBrowser::new(start) {
                        Ok(browser) => (
                            Step::CloneBrowse {
                                browser,
                                prior_input: input,
                            },
                            Continue,
                        ),
                        Err(e) => {
                            *message = Some(format!("error: {e:#}"));
                            (Step::ClonePath { input }, Continue)
                        }
                    }
                }
                KeyCode::Enter => match settings::load_clone_source(input.as_str()) {
                    Ok(draft) => {
                        self.draft = draft;
                        (
                            Step::Review {
                                selected: 0,
                                editing: None,
                            },
                            Continue,
                        )
                    }
                    Err(e) => {
                        *message = Some(format!("error: {e:#}"));
                        (Step::ClonePath { input }, Continue)
                    }
                },
                _ => {
                    input.on_key(key);
                    (Step::ClonePath { input }, Continue)
                }
            },

            Step::CloneBrowse {
                mut browser,
                prior_input,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    (Step::ClonePath { input: prior_input }, Continue)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if browser.selected + 1 < browser.entries.len() {
                        browser.selected += 1;
                    }
                    (
                        Step::CloneBrowse {
                            browser,
                            prior_input,
                        },
                        Continue,
                    )
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    browser.selected = browser.selected.saturating_sub(1);
                    (
                        Step::CloneBrowse {
                            browser,
                            prior_input,
                        },
                        Continue,
                    )
                }
                KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    if let Err(e) = browser.parent() {
                        *message = Some(format!("error: {e:#}"));
                    }
                    (
                        Step::CloneBrowse {
                            browser,
                            prior_input,
                        },
                        Continue,
                    )
                }
                KeyCode::Enter => {
                    let Some(entry) = browser.entries.get(browser.selected) else {
                        return (
                            Step::CloneBrowse {
                                browser,
                                prior_input,
                            },
                            Continue,
                        );
                    };
                    if entry.is_dir {
                        if let Err(e) = browser.descend() {
                            *message = Some(format!("error: {e:#}"));
                        }
                        return (
                            Step::CloneBrowse {
                                browser,
                                prior_input,
                            },
                            Continue,
                        );
                    }
                    match settings::load_clone_source(&entry.path.to_string_lossy()) {
                        Ok(draft) => {
                            self.draft = draft;
                            (
                                Step::Review {
                                    selected: 0,
                                    editing: None,
                                },
                                Continue,
                            )
                        }
                        Err(e) => {
                            *message = Some(format!("error: {e:#}"));
                            (
                                Step::CloneBrowse {
                                    browser,
                                    prior_input,
                                },
                                Continue,
                            )
                        }
                    }
                }
                _ => (
                    Step::CloneBrowse {
                        browser,
                        prior_input,
                    },
                    Continue,
                ),
            },

            Step::Location { selected } => match key.code {
                KeyCode::Esc => (Step::CloneAsk { yes: false }, Continue),
                KeyCode::Down | KeyCode::Char('j') => (
                    Step::Location {
                        selected: (selected + 1).min(LOCATION_PRESETS.len()),
                    },
                    Continue,
                ),
                KeyCode::Up | KeyCode::Char('k') => (
                    Step::Location {
                        selected: selected.saturating_sub(1),
                    },
                    Continue,
                ),
                KeyCode::Enter => {
                    if selected < LOCATION_PRESETS.len() {
                        self.draft.worktree_dir = LOCATION_PRESETS[selected].0.to_string();
                        (
                            Step::CopyFiles {
                                input: TextInput::default(),
                            },
                            Continue,
                        )
                    } else {
                        (
                            Step::LocationCustom {
                                input: TextInput::default(),
                            },
                            Continue,
                        )
                    }
                }
                _ => (Step::Location { selected }, Continue),
            },

            Step::LocationCustom { mut input } => match key.code {
                KeyCode::Esc => (
                    Step::Location {
                        selected: LOCATION_PRESETS.len(),
                    },
                    Continue,
                ),
                KeyCode::Enter => {
                    let path = input.trimmed();
                    self.draft.worktree_dir = if path.is_empty() {
                        DEFAULT_LOCATION.to_string()
                    } else {
                        path
                    };
                    (
                        Step::CopyFiles {
                            input: TextInput::default(),
                        },
                        Continue,
                    )
                }
                _ => {
                    input.on_key(key);
                    (Step::LocationCustom { input }, Continue)
                }
            },

            Step::CopyFiles { mut input } => match key.code {
                KeyCode::Esc => (Step::Location { selected: 0 }, Continue),
                KeyCode::Enter => {
                    self.draft.copy = settings::split_list(input.as_str());
                    (
                        Step::RunCommands {
                            commands: Vec::new(),
                            input: TextInput::default(),
                        },
                        Continue,
                    )
                }
                _ => {
                    input.on_key(key);
                    (Step::CopyFiles { input }, Continue)
                }
            },

            Step::RunCommands {
                mut commands,
                mut input,
            } => match key.code {
                KeyCode::Esc => (
                    Step::CopyFiles {
                        input: TextInput::with_value(self.draft.copy.join(", ")),
                    },
                    Continue,
                ),
                KeyCode::Enter => {
                    let cmd = input.trimmed();
                    if cmd.is_empty() {
                        self.draft.run = commands;
                        (
                            Step::Review {
                                selected: 0,
                                editing: None,
                            },
                            Continue,
                        )
                    } else {
                        commands.push(cmd);
                        (
                            Step::RunCommands {
                                commands,
                                input: TextInput::default(),
                            },
                            Continue,
                        )
                    }
                }
                _ => {
                    input.on_key(key);
                    (Step::RunCommands { commands, input }, Continue)
                }
            },

            Step::Review {
                selected,
                editing: Some(mut buf),
            } => match key.code {
                KeyCode::Esc => (
                    Step::Review {
                        selected,
                        editing: None,
                    },
                    Continue,
                ),
                KeyCode::Enter => {
                    self.commit_review_edit(selected, buf.as_str());
                    (
                        Step::Review {
                            selected,
                            editing: None,
                        },
                        Continue,
                    )
                }
                _ => {
                    buf.on_key(key);
                    (
                        Step::Review {
                            selected,
                            editing: Some(buf),
                        },
                        Continue,
                    )
                }
            },

            Step::Review {
                selected,
                editing: None,
            } => match key.code {
                KeyCode::Esc => (Step::CloneAsk { yes: false }, Continue),
                KeyCode::Down | KeyCode::Char('j') => (
                    Step::Review {
                        selected: (selected + 1).min(REVIEW_ROWS - 1),
                        editing: None,
                    },
                    Continue,
                ),
                KeyCode::Up | KeyCode::Char('k') => (
                    Step::Review {
                        selected: selected.saturating_sub(1),
                        editing: None,
                    },
                    Continue,
                ),
                KeyCode::Enter if selected == REVIEW_ROWS - 1 => (
                    Step::Review {
                        selected,
                        editing: None,
                    },
                    WizardOutcome::Done,
                ),
                KeyCode::Enter => {
                    let current = match selected {
                        0 => self.draft.worktree_dir.clone(),
                        1 => self.draft.copy.join(", "),
                        _ => self.draft.run.join(", "),
                    };
                    (
                        Step::Review {
                            selected,
                            editing: Some(TextInput::with_value(current)),
                        },
                        Continue,
                    )
                }
                _ => (
                    Step::Review {
                        selected,
                        editing: None,
                    },
                    Continue,
                ),
            },
        }
    }

    /// Stores an edited review row back into the draft.
    fn commit_review_edit(&mut self, row: usize, buf: &str) {
        match row {
            0 => {
                let value = buf.trim();
                self.draft.worktree_dir = if value.is_empty() {
                    DEFAULT_LOCATION.to_string()
                } else {
                    value.to_string()
                };
            }
            1 => self.draft.copy = settings::split_list(buf),
            2 => self.draft.run = settings::split_list(buf),
            _ => {}
        }
    }
}

/// One row of a `FileBrowser` listing.
#[derive(Debug)]
pub struct BrowserEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Navigable directory listing for picking a config file to clone: all
/// subdirectories plus `.toml` files.
#[derive(Debug)]
pub struct FileBrowser {
    pub dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
}

impl FileBrowser {
    /// Opens `dir` and lists its entries.
    pub fn new(dir: PathBuf) -> Result<FileBrowser> {
        let entries = read_entries(&dir)?;
        Ok(FileBrowser {
            dir,
            entries,
            selected: 0,
        })
    }

    /// Enters the selected directory; keeps the current listing on failure.
    pub fn descend(&mut self) -> Result<()> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(());
        };
        if !entry.is_dir {
            return Ok(());
        }
        let entries = read_entries(&entry.path)?;
        self.dir = entry.path.clone();
        self.entries = entries;
        self.selected = 0;
        Ok(())
    }

    /// Moves up to the parent directory; a no-op at the filesystem root.
    pub fn parent(&mut self) -> Result<()> {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        let entries = read_entries(&parent)?;
        self.dir = parent;
        self.entries = entries;
        self.selected = 0;
        Ok(())
    }
}

/// Lists directories and `.toml` files in `dir`: directories first, each
/// group alphabetical. Dotfiles are included since `.wtm.toml` is one.
fn read_entries(dir: &Path) -> Result<Vec<BrowserEntry>> {
    let read = std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    let mut entries = Vec::new();
    for item in read {
        let item = item.with_context(|| format!("cannot read {}", dir.display()))?;
        let path = item.path();
        let is_dir = path.is_dir();
        let name = item.file_name().to_string_lossy().to_string();
        if is_dir || name.ends_with(".toml") {
            entries.push(BrowserEntry { name, path, is_dir });
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(entries)
}

/// Marker used by `wtm init` and the TUI to decide whether setup is needed.
pub fn is_initialized(repo_root: &Path) -> bool {
    repo_root.join(CONFIG_FILE).exists()
}

/// Preview text for a location choice: the resolved directory, or the error.
pub fn location_preview(name: &str, repo_root: &Path) -> String {
    config::resolve_worktree_dir(name, repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(needs HOME set)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_lists_dirs_first_and_only_toml_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("beta")).unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::write(tmp.path().join("z.toml"), "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join(".wtm.toml"), "").unwrap();

        let browser = FileBrowser::new(tmp.path().to_path_buf()).unwrap();
        let names: Vec<&str> = browser.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", ".wtm.toml", "z.toml"]);
        assert!(browser.entries[0].is_dir);
        assert!(!browser.entries[2].is_dir);
    }

    #[test]
    fn browser_descends_and_returns_to_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.toml"), "").unwrap();

        let mut browser = FileBrowser::new(tmp.path().to_path_buf()).unwrap();
        assert_eq!(browser.entries[0].name, "sub");
        browser.descend().unwrap();
        assert_eq!(browser.dir, sub);
        assert_eq!(browser.entries[0].name, "inner.toml");
        // Enter on a file is a no-op at the browser level.
        browser.descend().unwrap();
        assert_eq!(browser.dir, sub);
        browser.parent().unwrap();
        assert_eq!(browser.dir, tmp.path());
    }

    #[test]
    fn browser_errors_on_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = FileBrowser::new(tmp.path().join("nope")).unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }
}
