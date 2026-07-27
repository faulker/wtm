//! Desktop integration: handing a file to the OS's default application and
//! putting text on the system clipboard.
//!
//! Both shell out to the platform's standard tool rather than pulling in a
//! crate, matching how the rest of the app shells out to `git` and `curl`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

// Requests recorded instead of performed while testing. A test run must not
// open files on the developer's desktop or overwrite their clipboard, so both
// entry points divert here under `cfg(test)` and tests assert on what was asked
// for. Thread-local so parallel tests don't see each other's requests.
#[cfg(test)]
thread_local! {
    static RECORDED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Takes everything recorded on this thread since the last call.
#[cfg(test)]
pub fn take_recorded() -> Vec<String> {
    RECORDED.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

#[cfg(test)]
fn record(request: String) {
    RECORDED.with(|r| r.borrow_mut().push(request));
}

/// Opens `path` with whatever the OS considers its default application, the
/// double-click equivalent. Returns as soon as the handler is launched; a
/// handler that fails after that is out of our hands.
pub fn open_path(path: &Path) -> Result<()> {
    if cfg!(test) {
        #[cfg(test)]
        record(format!("open {}", path.display()));
        return Ok(());
    }
    let (program, args) = opener();
    let Some(program) = program else {
        bail!("don't know how to open files on this platform");
    };
    let status = Command::new(program)
        .args(args)
        .arg(path)
        // Detach from our stdio: the TUI owns the terminal, so a handler that
        // prints (or is itself a terminal program) must not draw over it.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} could not open {}", path.display());
    }
    Ok(())
}

/// The platform's "open with the default app" command and any leading
/// arguments it needs.
fn opener() -> (Option<&'static str>, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        (Some("open"), &[])
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin, and its first quoted argument is the window
        // title, so an empty one has to be passed before the path.
        (Some("cmd"), &["/C", "start", ""])
    } else {
        (Some("xdg-open"), &[])
    }
}

/// Copies `text` to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    if cfg!(test) {
        #[cfg(test)]
        record(format!("copy {text}"));
        return Ok(());
    }
    let candidates = clipboard_commands();
    let mut last: Option<anyhow::Error> = None;
    for (program, args) in candidates {
        match pipe_to(program, args, text) {
            Ok(()) => return Ok(()),
            // Try the next candidate: on Linux which of wl-copy/xclip/xsel
            // exists depends on the session, so a missing one isn't fatal.
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no clipboard tool available")))
}

/// Clipboard tools to try, in order of preference for the platform.
fn clipboard_commands() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        vec![("pbcopy", &[][..])]
    } else if cfg!(target_os = "windows") {
        vec![("clip", &[][..])]
    } else {
        vec![
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ]
    }
}

/// Runs `program` with `text` on its stdin and waits for it to finish.
fn pipe_to(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    child
        .stdin
        .as_mut()
        .context("clipboard command closed its input")?
        .write_all(text.as_bytes())
        .with_context(|| format!("cannot write to {program}"))?;
    let status = child
        .wait()
        .with_context(|| format!("cannot wait for {program}"))?;
    if !status.success() {
        bail!("{program} failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform tables must always name a tool, so the callers' error paths
    /// only ever report a genuinely missing binary.
    #[test]
    fn platform_tables_are_populated() {
        assert!(opener().0.is_some());
        assert!(!clipboard_commands().is_empty());
    }

    /// `pipe_to` reports the command's own failure rather than silently
    /// succeeding, and feeds it the text on stdin.
    #[test]
    fn pipe_to_reports_failure_and_writes_stdin() {
        // `cat` into nowhere succeeds; `false` never does.
        pipe_to("cat", &[], "hello").unwrap();
        assert!(pipe_to("false", &[], "hello").is_err());
        assert!(pipe_to("definitely-not-a-real-binary", &[], "x").is_err());
    }

    /// Under test both entry points record instead of touching the desktop.
    #[test]
    fn test_builds_record_instead_of_acting() {
        take_recorded();
        open_path(Path::new("/tmp/some file.txt")).unwrap();
        copy_to_clipboard("src/main.rs").unwrap();
        assert_eq!(
            take_recorded(),
            vec!["open /tmp/some file.txt", "copy src/main.rs"]
        );
        // Draining leaves nothing behind for the next test on this thread.
        assert!(take_recorded().is_empty());
    }
}
