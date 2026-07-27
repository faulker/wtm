//! First-run setup wizard shown when the repo has no `.wtm.toml` yet.
//!
//! A welcome screen explains what setup is for and offers two routes: answer
//! three questions (where worktrees go, which local files to copy into them,
//! what to run once they exist), or copy the answers wholesale from another repo
//! that already uses wtm. Both routes end on a review screen where every
//! setting can still be edited before `.wtm.toml` is written.
//!
//! Every question screen carries a short "why this matters" blurb and a
//! `step N of M` label, and Esc always steps back exactly one screen.

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
    /// True once the draft came from another repo's config. Only affects the
    /// step counter (the clone route is shorter) and the review screen's note.
    pub cloned: bool,
    /// Raw text the user last entered for the copy-files question. Stepping back
    /// restores exactly what they typed, so an answer they deliberately cleared
    /// isn't replaced by the repo's suggestions again.
    copy_answer: Option<String>,
    /// Whether the commands question has been answered, for the same reason.
    run_answered: bool,
}

/// Which wizard screen is showing.
pub enum Step {
    /// What wtm does, what setup writes, and which of the two routes to take.
    Welcome { selected: usize },
    /// Typed path to a repo or `.wtm.toml` to copy settings from.
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

impl Step {
    /// Identifies the step in test failure messages.
    #[cfg(test)]
    pub fn name(&self) -> &'static str {
        match self {
            Step::Welcome { .. } => "welcome",
            Step::ClonePath { .. } => "clone path",
            Step::CloneBrowse { .. } => "clone browser",
            Step::Location { .. } => "location",
            Step::LocationCustom { .. } => "location path",
            Step::CopyFiles { .. } => "copy files",
            Step::RunCommands { .. } => "run commands",
            Step::Review { .. } => "review",
        }
    }
}

/// Rows on the review screen, in order.
pub const REVIEW_ROWS: usize = 4;

/// The two routes offered on the welcome screen, in order.
pub const WELCOME_OPTIONS: &[(&str, &str)] = &[
    ("Set this repo up now", "three quick questions"),
    (
        "Copy settings from another repo",
        "reuse another repo's .wtm.toml",
    ),
];

/// What a key press did, for the app to act on.
pub enum WizardOutcome {
    Continue,
    /// The draft is final; write it and enter the normal list view.
    Done,
    Quit,
}

impl SetupWizard {
    /// Starts the wizard on the welcome screen.
    pub fn new(repo_root: PathBuf) -> SetupWizard {
        SetupWizard {
            repo_root,
            // Default to setting the repo up here: most repos are the first.
            step: Step::Welcome { selected: 0 },
            draft: ConfigDraft::default(),
            cloned: false,
            copy_answer: None,
            run_answered: false,
        }
    }

    /// A `step N of M` label for the panel title, so the user can tell how much
    /// is left. The clone route is shorter than answering the questions, so the
    /// total depends on which route was taken.
    pub fn progress(&self) -> String {
        let (n, of) = match self.step {
            Step::Welcome { .. } => return "welcome".to_string(),
            Step::ClonePath { .. } | Step::CloneBrowse { .. } => (1, 2),
            Step::Location { .. } | Step::LocationCustom { .. } => (1, 4),
            Step::CopyFiles { .. } => (2, 4),
            Step::RunCommands { .. } => (3, 4),
            Step::Review { .. } if self.cloned => (2, 2),
            Step::Review { .. } => (4, 4),
        };
        format!("step {n} of {of}")
    }

    /// Handles one key press. Errors (bad clone path, unreadable directory)
    /// land in `message` and keep the current step on screen.
    pub fn on_key(&mut self, key: KeyEvent, message: &mut Option<String>) -> WizardOutcome {
        // Take the step by value so transitions can move state between steps.
        let step = std::mem::replace(&mut self.step, Step::Welcome { selected: 0 });
        let (next, outcome) = self.handle(step, key, message);
        self.step = next;
        outcome
    }

    /// The location question. Entered from the welcome screen and from Esc on
    /// the next step, always with the first preset highlighted.
    fn location_step() -> Step {
        Step::Location { selected: 0 }
    }

    /// The copy-files question, pre-filled with the local config files found in
    /// the repo root (or whatever the user already answered).
    fn copy_files_step(&self) -> Step {
        let value = match &self.copy_answer {
            Some(answer) => answer.clone(),
            None => settings::suggest_copy_files(&self.repo_root).join(", "),
        };
        Step::CopyFiles {
            input: TextInput::with_value(value),
        }
    }

    /// The setup-commands question. The first line is pre-filled with the
    /// install command inferred from the repo's lockfiles, and any commands
    /// already answered are listed above it.
    fn run_commands_step(&self) -> Step {
        let mut commands = self.draft.run.clone();
        // Only guess when the user hasn't answered this yet; coming back from
        // Review must not re-add a suggestion they deliberately removed.
        let suggested = if self.run_answered {
            Vec::new()
        } else {
            settings::suggest_run_commands(&self.repo_root)
        };
        // Everything but the last suggestion goes into the list; the last one
        // sits in the input so it is obvious it can be edited or cleared.
        let mut input = String::new();
        if let Some((last, rest)) = suggested.split_last() {
            commands.extend(rest.iter().cloned());
            input = last.clone();
        }
        Step::RunCommands {
            commands,
            input: TextInput::with_value(input),
        }
    }

    /// The review screen, reached from either route.
    fn review_step() -> Step {
        Step::Review {
            selected: 0,
            editing: None,
        }
    }

    fn handle(
        &mut self,
        step: Step,
        key: KeyEvent,
        message: &mut Option<String>,
    ) -> (Step, WizardOutcome) {
        use WizardOutcome::Continue;
        match step {
            // The welcome screen is a two-item menu: arrows (or 1/2) choose,
            // Enter commits. Esc quits, since there is nothing behind it.
            Step::Welcome { selected } => match key.code {
                KeyCode::Down
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::Char('j')
                | KeyCode::Char('l') => (
                    Step::Welcome {
                        selected: (selected + 1).min(WELCOME_OPTIONS.len() - 1),
                    },
                    Continue,
                ),
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h') => (
                    Step::Welcome {
                        selected: selected.saturating_sub(1),
                    },
                    Continue,
                ),
                KeyCode::Char('1') => (Self::location_step(), Continue),
                KeyCode::Char('2') => (
                    Step::ClonePath {
                        input: TextInput::default(),
                    },
                    Continue,
                ),
                KeyCode::Enter if selected == 0 => (Self::location_step(), Continue),
                KeyCode::Enter => (
                    Step::ClonePath {
                        input: TextInput::default(),
                    },
                    Continue,
                ),
                KeyCode::Esc | KeyCode::Char('q') => {
                    (Step::Welcome { selected }, WizardOutcome::Quit)
                }
                _ => (Step::Welcome { selected }, Continue),
            },

            Step::ClonePath { mut input } => match key.code {
                KeyCode::Esc => (Step::Welcome { selected: 1 }, Continue),
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
                        self.cloned = true;
                        (Self::review_step(), Continue)
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
                            self.cloned = true;
                            (Self::review_step(), Continue)
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
                KeyCode::Esc => (Step::Welcome { selected: 0 }, Continue),
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
                        (self.copy_files_step(), Continue)
                    } else {
                        (
                            Step::LocationCustom {
                                input: TextInput::with_value(self.draft.worktree_dir.clone()),
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
                    (self.copy_files_step(), Continue)
                }
                _ => {
                    input.on_key(key);
                    (Step::LocationCustom { input }, Continue)
                }
            },

            Step::CopyFiles { mut input } => match key.code {
                KeyCode::Esc => (Self::location_step(), Continue),
                KeyCode::Enter => {
                    self.draft.copy = settings::split_list(input.as_str());
                    self.copy_answer = Some(input.as_str().to_string());
                    (self.run_commands_step(), Continue)
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
                KeyCode::Esc => (self.copy_files_step(), Continue),
                // Backspace on an empty line takes back the command above it, so
                // a typo can be fixed without leaving the screen.
                KeyCode::Backspace if input.as_str().is_empty() => {
                    let restored = commands.pop().unwrap_or_default();
                    (
                        Step::RunCommands {
                            commands,
                            input: TextInput::with_value(restored),
                        },
                        Continue,
                    )
                }
                KeyCode::Enter => {
                    let cmd = input.trimmed();
                    if cmd.is_empty() {
                        self.draft.run = commands;
                        self.run_answered = true;
                        (Self::review_step(), Continue)
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
                // Esc steps back one screen like everywhere else in the wizard:
                // to the clone path on the clone route, or to the last question
                // on the other, keeping the answers already given.
                KeyCode::Esc if self.cloned => (
                    Step::ClonePath {
                        input: TextInput::default(),
                    },
                    Continue,
                ),
                KeyCode::Esc => (self.run_commands_step(), Continue),
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

    /// Stores an edited review row back into the draft. Edits here also count as
    /// answering the matching question, so stepping back doesn't overwrite them
    /// with the repo's suggestions.
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
            1 => {
                self.draft.copy = settings::split_list(buf);
                self.copy_answer = Some(buf.to_string());
            }
            2 => {
                self.draft.run = settings::split_list(buf);
                self.run_answered = true;
            }
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

/// How a `worktree_dir` value reads on the review screen: the preset's plain
/// label where there is one, otherwise the path as typed.
pub fn location_label(value: &str) -> &str {
    LOCATION_PRESETS
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, label)| *label)
        .unwrap_or(value)
}

/// Preview text for a location choice: the resolved directory, or the error.
/// `..` segments are folded away first, since the `sibling` preset resolves to
/// `<repo>/../<repo>-worktrees` and showing that literally is just noise.
pub fn location_preview(name: &str, repo_root: &Path) -> String {
    config::resolve_worktree_dir(name, repo_root)
        .map(|p| tidy_path(&p))
        .unwrap_or_else(|_| "(needs HOME set)".to_string())
}

/// Lexically removes `x/..` pairs from a path for display. Purely textual (the
/// directory doesn't exist yet, so it can't be canonicalized) and never leaves
/// the path shorter than its root.
fn tidy_path(path: &Path) -> String {
    use std::path::Component;
    let mut parts: Vec<Component> = Vec::new();
    for part in path.components() {
        match part {
            Component::ParentDir if matches!(parts.last(), Some(Component::Normal(_))) => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect::<PathBuf>().display().to_string()
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

    /// The `sibling` preset resolves through a `..`, which the preview folds
    /// away so the wizard shows a path a person would recognise.
    #[test]
    fn location_preview_folds_away_parent_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        let preview = location_preview("sibling", &repo);
        assert_eq!(
            preview,
            tmp.path().join("proj-worktrees").display().to_string()
        );
        assert!(!preview.contains(".."), "{preview}");
        // Paths with nothing to fold pass through, and a leading `..` (nothing
        // above it to cancel) is kept rather than dropped.
        assert_eq!(tidy_path(Path::new("/a/b/c")), "/a/b/c");
        assert_eq!(tidy_path(Path::new("../x")), "../x");
        assert_eq!(tidy_path(Path::new("/a/b/../../c")), "/c");
    }

    #[test]
    fn browser_errors_on_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = FileBrowser::new(tmp.path().join("nope")).unwrap_err();
        assert!(err.to_string().contains("cannot read"));
    }
}
