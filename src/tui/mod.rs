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
/// A self-update sets `App::restart_exe`; the hand-off happens here, after the
/// terminal has been restored, so the new binary starts from a clean terminal.
/// Terminal-mode commands (a `conflict_editor` or an `open_command`) don't end
/// the run: the event loop suspends the TUI for them and comes back.
pub fn run(ctx: Ctx) -> Result<()> {
    let mut app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    // Mouse reporting lets the diff, log, and resolver views respond to clicks
    // and the scroll wheel. On terminals that support the Kitty keyboard
    // protocol (Ghostty, kitty, WezTerm, foot, recent iTerm2) the enhancement
    // flags make modified keys like Shift+Up/Down report their modifier
    // reliably instead of looking like a bare arrow key.
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    enter_extras(enhanced);
    let result = event_loop(&mut terminal, &mut app, enhanced);
    leave_extras(enhanced);
    ratatui::restore();
    result?;
    if let Some(exe) = &app.restart_exe {
        println!("restarting {}", exe.display());
        crate::update::restart(exe)?;
    }
    Ok(())
}

/// Turns on what `ratatui::init` leaves off: mouse reporting, and the Kitty
/// keyboard protocol where the terminal has it.
fn enter_extras(enhanced: bool) {
    let _ = write_stdout(MOUSE_ON);
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

/// Undoes [`enter_extras`], before `ratatui::restore` hands the terminal back.
fn leave_extras(enhanced: bool) {
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = write_stdout(MOUSE_OFF);
}

/// Runs a terminal-mode command (a `conflict_editor` or an `open_command`)
/// with the TUI suspended: the terminal is restored so the program gets it to
/// itself, and once it exits the TUI is set up again, redrawn from scratch,
/// and told how it went.
fn suspend_for_command(
    terminal: &mut DefaultTerminal,
    enhanced: bool,
    cmd: &str,
    dir: &str,
) -> Result<std::process::ExitStatus, String> {
    leave_extras(enhanced);
    ratatui::restore();
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("failed to run '{cmd}' in {dir}: {e}"));
    *terminal = ratatui::init();
    enter_extras(enhanced);
    let _ = terminal.clear();
    result
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
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, enhanced: bool) -> Result<()> {
    while !app.quit {
        // A terminal program asked for the screen: give it up, wait, take it
        // back. Done here rather than in the key handler so the app never
        // touches the terminal itself.
        if let Some(suspend) = app.suspend_for.take() {
            let result = suspend_for_command(terminal, enhanced, &suspend.cmd, &suspend.dir);
            app.resume_after_suspend(suspend.reason, result);
        }
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
