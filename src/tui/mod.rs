//! Interactive terminal UI built on ratatui.

mod app;
mod config_editor;
mod help;
mod highlight;
mod setup;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

use crate::ops::Ctx;
use app::App;

/// Runs the interactive TUI until the user quits.
pub fn run(ctx: Ctx) -> Result<()> {
    let app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    // Mouse capture lets the diff and log views respond to the scroll wheel.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    // On terminals that support the Kitty keyboard protocol (Ghostty, kitty,
    // WezTerm, foot, recent iTerm2) this makes modified keys like Shift+Up/Down
    // report their modifier reliably instead of looking like a bare arrow key.
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = event_loop(&mut terminal, app);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Draw/input loop. Polls with a timeout so background create progress keeps
/// the screen updating even without keypresses.
fn event_loop(terminal: &mut DefaultTerminal, mut app: App) -> Result<()> {
    while !app.quit {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => {}
            }
        }
    }
    Ok(())
}
