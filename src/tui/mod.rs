//! Interactive terminal UI built on ratatui.

mod app;
mod config_editor;
mod setup;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::ops::Ctx;
use app::App;

/// Runs the interactive TUI until the user quits.
pub fn run(ctx: Ctx) -> Result<()> {
    let app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app);
    ratatui::restore();
    result
}

/// Draw/input loop. Polls with a timeout so background create progress keeps
/// the screen updating even without keypresses.
fn event_loop(terminal: &mut DefaultTerminal, mut app: App) -> Result<()> {
    while !app.quit {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
    }
    Ok(())
}
