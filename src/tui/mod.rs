//! Interactive terminal UI built on ratatui.

mod app;
mod config_editor;
mod help;
mod highlight;
mod setup;
mod theme;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;

/// Turns on button and wheel reporting (`?1000h`) with SGR-encoded coordinates
/// (`?1006h`), and nothing else.
///
/// Deliberately not crossterm's `EnableMouseCapture`, which also turns on drag
/// (`?1002h`) and any-motion (`?1003h`) tracking. wtm only ever acts on clicks
/// and the wheel, and motion tracking means the terminal sends an event for
/// every cell the pointer crosses — each one waking the event loop for a full
/// redraw of a screen that did not change. Merely moving the mouse across the
/// window was enough to peg wtm (and the terminal drawing it) for as long as
/// the pointer kept moving.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";

/// Undoes `MOUSE_ON`, innermost mode first.
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

use crate::ops::Ctx;
use app::App;

/// Runs the interactive TUI until the user quits.
///
/// A self-update sets `App::restart_exe` and a terminal-mode open command sets
/// `App::exec_on_exit`; both hand-offs happen here, after the terminal has been
/// restored, so the program that takes over starts from a clean terminal.
pub fn run(ctx: Ctx) -> Result<()> {
    let mut app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    // Mouse reporting lets the diff, log, and resolver views respond to clicks
    // and the scroll wheel.
    let _ = write_stdout(MOUSE_ON);
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
    let result = event_loop(&mut terminal, &mut app);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = write_stdout(MOUSE_OFF);
    ratatui::restore();
    result?;
    if let Some(exe) = &app.restart_exe {
        println!("restarting {}", exe.display());
        crate::update::restart(exe)?;
    }
    if let Some((cmd, dir)) = &app.exec_on_exit {
        return exec_in_terminal(cmd, dir);
    }
    Ok(())
}

/// Writes a terminal control string straight to stdout, flushed. Used for the
/// mouse modes, which are set by hand rather than through crossterm's
/// all-or-nothing capture command.
fn write_stdout(s: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// Runs a `CommandMode::Terminal` open command in place of the TUI, inheriting
/// this (already restored) terminal so an interactive program can use it. wtm
/// exits with the command's own status.
fn exec_in_terminal(cmd: &str, dir: &str) -> Result<()> {
    use anyhow::Context;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .status()
        .with_context(|| format!("failed to run '{cmd}' in {dir}"))?;
    std::process::exit(status.code().unwrap_or(0));
}

/// How long the loop waits for input while something on screen is moving: a
/// spinner frame, or a background result a tick has to pick up.
const ACTIVE_POLL: Duration = Duration::from_millis(100);

/// How long it waits when nothing is. Input still wakes the loop the instant it
/// arrives, so this only sets how often an untouched screen repaints itself —
/// ten frames a second of an identical screen is what keeps a terminal (and the
/// GPU behind it) burning battery with wtm just sitting open.
const IDLE_POLL: Duration = Duration::from_millis(750);

/// And how long once nobody has touched the app at all for a while (see
/// `App::is_idle`). A pane left open in a background window has nothing to
/// repaint for; the first keypress puts it straight back on `IDLE_POLL`.
const PARKED_POLL: Duration = Duration::from_millis(2000);

/// Draw/input loop. Polls with a timeout so background create progress keeps
/// the screen updating even without keypresses.
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        // Drain any already-queued input before tick/draw. A slow refresh or
        // syntect pass must not delay Back/q; otherwise a second press buffered
        // during the stall can quit once the pop finally lands.
        if event::poll(Duration::ZERO)? {
            dispatch_event(app, event::read()?)?;
            continue;
        }
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        let timeout = if app.needs_fast_tick() {
            ACTIVE_POLL
        } else if app.is_idle() {
            PARKED_POLL
        } else {
            IDLE_POLL
        };
        if event::poll(timeout)? {
            dispatch_event(app, event::read()?)?;
        }
    }
    Ok(())
}

/// Routes one crossterm event into the app.
fn dispatch_event(app: &mut App, ev: Event) -> Result<()> {
    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
        Event::Mouse(mouse) => app.on_mouse(mouse),
        _ => {}
    }
    Ok(())
}
