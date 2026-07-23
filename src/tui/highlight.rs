//! Syntax-highlighted rendering of unified diff text for the TUI.
//!
//! Added and removed lines get a green/red background tint across the whole
//! line, and the code itself is colored with syntect (theme: base16-ocean.dark)
//! based on the file's extension. Headers (`diff --git`, `+++`/`---`, `@@`)
//! keep the app's existing accent styling.

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Background tint for added lines (dark green, dark-terminal friendly).
const ADD_BG: Color = Color::Rgb(16, 60, 30);
/// Background tint for removed lines (dark red).
const DEL_BG: Color = Color::Rgb(72, 24, 24);
/// Accent for hunk headers, matching the UI accent.
const ACCENT: Color = Color::Cyan;
/// Diffs longer than this skip syntect (it is O(content) per parse) and fall
/// back to plain green/red coloring so huge diffs never stall the redraw.
const MAX_HIGHLIGHT_LINES: usize = 4000;

/// The bundled syntax definitions, loaded once per process.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The color theme used for code, loaded once per process.
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("syntect ships base16-ocean.dark")
    })
}

thread_local! {
    /// One-entry render cache. Only one diff is ever on screen, and its text
    /// changes rarely (on file switch or refresh) compared to how often the
    /// frame redraws, so caching the last render avoids re-highlighting the
    /// same content on every tick.
    static CACHE: RefCell<Option<(u64, Vec<Line<'static>>)>> = const { RefCell::new(None) };
}

/// Renders unified diff text for `path` as styled lines: syntax-highlighted
/// code with green/red line backgrounds for additions/removals. Results are
/// memoized on (path, content), so calling this every frame is cheap.
pub fn diff_lines(path: &str, content: &str) -> Vec<Line<'static>> {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    content.hash(&mut hasher);
    let key = hasher.finish();
    CACHE.with(|c| {
        if let Some((k, lines)) = c.borrow().as_ref()
            && *k == key
        {
            return lines.clone();
        }
        let lines = render(path, content);
        *c.borrow_mut() = Some((key, lines.clone()));
        lines
    })
}

/// Renders the diff without caching. Split out so tests can call it directly.
fn render(path: &str, content: &str) -> Vec<Line<'static>> {
    // Pick a syntax by file extension (then by full name, e.g. `Makefile`);
    // fall back to no highlighting when the file type is unknown or the diff
    // is too large to highlight without janking the UI.
    let ext = path.rsplit('.').next().unwrap_or("");
    let name = path.rsplit('/').next().unwrap_or(path);
    let syntax = syntax_set()
        .find_syntax_by_extension(ext)
        .or_else(|| syntax_set().find_syntax_by_extension(name));
    let mut highlighter = match syntax {
        Some(s) if content.lines().count() <= MAX_HIGHLIGHT_LINES => {
            Some(HighlightLines::new(s, theme()))
        }
        _ => None,
    };
    content
        .lines()
        .map(|line| diff_line(line, &mut highlighter))
        .collect()
}

/// Styles one diff line: headers keep their accent colors; `+`/`-` lines get
/// a full-line background tint with the code highlighted on top.
fn diff_line(line: &str, highlighter: &mut Option<HighlightLines<'static>>) -> Line<'static> {
    // Header lines, in the same precedence as before highlighting existed.
    if line.starts_with("+++") || line.starts_with("---") {
        return Line::styled(line.to_string(), Style::new().add_modifier(Modifier::BOLD));
    }
    if line.starts_with("@@") {
        return Line::styled(line.to_string(), Style::new().fg(ACCENT));
    }
    if line.starts_with("diff --git") {
        return Line::styled(
            line.to_string(),
            Style::new().add_modifier(Modifier::BOLD).fg(Color::Magenta),
        );
    }
    // Code lines: a marker column (+/-/space) followed by the code itself.
    let (marker, code, bg, marker_fg) = match line.as_bytes().first() {
        Some(b'+') => ("+", &line[1..], Some(ADD_BG), Some(Color::Green)),
        Some(b'-') => ("-", &line[1..], Some(DEL_BG), Some(Color::Red)),
        Some(b' ') => (" ", &line[1..], None, None),
        // Anything else ("index …", "new file mode …", "\ No newline…").
        _ => return Line::styled(line.to_string(), Style::new()),
    };
    let base = bg.map(|b| Style::new().bg(b)).unwrap_or_default();
    let mut spans = vec![Span::styled(
        marker.to_string(),
        marker_fg
            .map(|f| base.fg(f).add_modifier(Modifier::BOLD))
            .unwrap_or(base),
    )];
    match highlighter
        .as_mut()
        .and_then(|h| h.highlight_line(code, syntax_set()).ok())
    {
        Some(regions) => {
            // Keep syntect's foreground colors but replace its background with
            // the diff tint (or the terminal default on context lines).
            for (style, text) in regions {
                let fg = style.foreground;
                spans.push(Span::styled(
                    text.to_string(),
                    base.fg(Color::Rgb(fg.r, fg.g, fg.b)),
                ));
            }
        }
        None => {
            // No syntax known: fall back to plain green/red foregrounds so
            // added/removed lines still read at a glance.
            let fg = marker_fg.map(|f| base.fg(f)).unwrap_or(base);
            spans.push(Span::styled(code.to_string(), fg));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_and_removed_lines_get_background_tints() {
        let diff = "diff --git a/x.rs b/x.rs\n@@ -1,2 +1,2 @@\n-let a = 1;\n+let a = 2;\n context";
        let lines = render("x.rs", diff);
        assert_eq!(lines.len(), 5);
        // The header keeps its magenta bold styling with no tint.
        assert_eq!(lines[0].style.fg, Some(Color::Magenta));
        // The hunk header uses the accent.
        assert_eq!(lines[1].style.fg, Some(Color::Cyan));
        // Every span of a removed line carries the red tint, added the green.
        assert!(lines[2].spans.iter().all(|s| s.style.bg == Some(DEL_BG)));
        assert!(lines[3].spans.iter().all(|s| s.style.bg == Some(ADD_BG)));
        // Context lines keep the terminal background.
        assert!(lines[4].spans.iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn known_extensions_get_syntax_colors() {
        let diff = "@@ -0,0 +1 @@\n+fn main() {}";
        let lines = render("x.rs", diff);
        // The added code is split into multiple highlighted spans (marker plus
        // at least keyword/identifier regions), not one flat green span.
        assert!(
            lines[1].spans.len() > 2,
            "expected syntax regions, got {:?}",
            lines[1].spans
        );
    }

    #[test]
    fn unknown_extensions_fall_back_to_plain_colors() {
        let diff = "@@ -0,0 +1 @@\n+hello\n-goodbye";
        let lines = render("file.zzz-unknown", diff);
        // Marker + one code span, colored green/red by prefix.
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Green));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn diff_lines_is_cached_and_stable() {
        let diff = "@@ -0,0 +1 @@\n+let x = 1;";
        let first = diff_lines("a.rs", diff);
        let second = diff_lines("a.rs", diff);
        assert_eq!(first, second);
    }
}
