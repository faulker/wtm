//! TUI application state and key handling.

use std::sync::mpsc::{Receiver, channel};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::ops::WorktreeInfo;
use crate::ops::{self, Ctx};

/// Message from the background create thread.
pub enum CreateMsg {
    Progress(String),
    Done(Result<crate::ops::CreateResult, String>),
}

/// Which screen/overlay is active.
pub enum View {
    List,
    /// Scrollable diff of one worktree's uncommitted changes.
    Diff {
        name: String,
        content: String,
        scroll: u16,
    },
    /// Branch-name input for a new worktree.
    Create {
        input: String,
    },
    /// Progress of an in-flight create running on a background thread.
    Creating {
        branch: String,
        lines: Vec<String>,
        rx: Receiver<CreateMsg>,
        done: bool,
    },
    /// Delete confirmation; `dirty` is the number of uncommitted changes.
    ConfirmDelete {
        name: String,
        dirty: usize,
    },
    Help,
}

pub struct App {
    pub ctx: Ctx,
    pub worktrees: Vec<WorktreeInfo>,
    pub selected: usize,
    pub view: View,
    /// One-line status or error shown at the bottom.
    pub message: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(ctx: Ctx) -> anyhow::Result<App> {
        let mut app = App {
            ctx,
            worktrees: Vec::new(),
            selected: 0,
            view: View::List,
            message: None,
            quit: false,
        };
        app.refresh();
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

    /// Drains progress messages from an in-flight create.
    pub fn tick(&mut self) {
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

    pub fn on_key(&mut self, key: KeyEvent) {
        self.message = None;
        match &mut self.view {
            View::List => self.on_list_key(key),
            View::Diff { scroll, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.view = View::List,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(20),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
                KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                _ => {}
            },
            View::Create { input } => match key.code {
                KeyCode::Esc => self.view = View::List,
                KeyCode::Enter => {
                    let branch = input.trim().to_string();
                    if !branch.is_empty() {
                        self.start_create(branch);
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
            View::Creating { done, .. } => {
                // Ignore keys while setup runs; Enter/Esc dismisses when done.
                if *done && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::List;
                    self.refresh();
                }
            }
            View::ConfirmDelete { name, dirty } => match key.code {
                KeyCode::Char('y') if *dirty == 0 => {
                    let name = name.clone();
                    self.remove(&name, false);
                }
                KeyCode::Char('f') => {
                    let name = name.clone();
                    self.remove(&name, true);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.view = View::List,
                _ => {}
            },
            View::Help => self.view = View::List,
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
            KeyCode::Char('n') => {
                self.view = View::Create {
                    input: String::new(),
                }
            }
            KeyCode::Char('d') => {
                if let Some(wt) = self.selected_worktree() {
                    if wt.is_main {
                        self.message = Some("cannot remove the main worktree".to_string());
                    } else {
                        self.view = View::ConfirmDelete {
                            name: wt.name.clone(),
                            dirty: wt.dirty,
                        };
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(wt) = self.selected_worktree() {
                    let name = wt.name.clone();
                    match ops::diff(&self.ctx, &name) {
                        Ok((_, content)) => {
                            self.view = View::Diff {
                                name,
                                content,
                                scroll: 0,
                            };
                        }
                        Err(e) => self.message = Some(format!("error: {e:#}")),
                    }
                }
            }
            KeyCode::Char('?') => self.view = View::Help,
            _ => {}
        }
    }

    /// Kicks off `ops::create` on a background thread so setup commands
    /// (npm install etc.) don't freeze the UI.
    fn start_create(&mut self, branch: String) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let thread_branch = branch.clone();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = ops::create(&ctx, &thread_branch, None, move |line| {
                let _ = progress_tx.send(CreateMsg::Progress(line.to_string()));
            });
            let _ = tx.send(CreateMsg::Done(result.map_err(|e| format!("{e:#}"))));
        });
        self.view = View::Creating {
            branch,
            lines: Vec::new(),
            rx,
            done: false,
        };
    }

    fn remove(&mut self, name: &str, force: bool) {
        match ops::remove(&self.ctx, name, force, false) {
            Ok(info) => self.message = Some(format!("removed '{}'", info.name)),
            Err(e) => self.message = Some(format!("error: {e:#}")),
        }
        self.view = View::List;
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    /// Builds a real single-commit git repo so App can list worktrees.
    fn test_app() -> (tempfile::TempDir, App) {
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
        let app = App::new(Ctx::discover(&repo).unwrap()).unwrap();
        (tmp, app)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
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
            View::Create { input } => assert_eq!(input, "a"),
            _ => panic!("expected create dialog"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
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
        assert!(matches!(app.view, View::Diff { .. }));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::PageDown);
        match &app.view {
            View::Diff { scroll, .. } => assert_eq!(*scroll, 21),
            _ => panic!("expected diff view"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.view, View::List));
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
