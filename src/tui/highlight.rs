//! Syntax-highlighted rendering of unified diff text for the TUI.
//!
//! Added and removed lines get a green/red background tint across the whole
//! line, and the code itself is colored with syntect based on the file's
//! extension. The syntect theme is selectable (see [`DIFF_THEMES`]); the
//! default is warmer and higher-contrast than base16-ocean.dark, which reads
//! as a muddy blue-gray on many dark terminals. Headers (`diff --git`,
//! `+++`/`---`, `@@`) keep the app's existing accent styling.

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::theme;

/// Background tint for added lines (dark green, dark-terminal friendly).
const ADD_BG: Color = Color::Rgb(16, 60, 30);
/// Background tint for removed lines (dark red).
const DEL_BG: Color = Color::Rgb(72, 24, 24);
/// Accent for hunk headers, matching the UI accent.
const ACCENT: Color = Color::Cyan;
/// Diffs longer than this skip syntect (it is O(content) per parse) and fall
/// back to plain green/red coloring so huge diffs never stall the redraw.
const MAX_HIGHLIGHT_LINES: usize = 4000;

/// Selectable syntect themes for the diff pane: short id (stored in config),
/// display label, and the syntect theme name.
pub const DIFF_THEMES: &[(&str, &str, &str)] = &[
    ("eighties", "Eighties", "base16-eighties.dark"),
    ("mocha", "Mocha", "base16-mocha.dark"),
    ("ocean", "Ocean", "base16-ocean.dark"),
    ("solarized", "Solarized", "Solarized (dark)"),
    ("github", "GitHub", "InspiredGitHub"),
];

/// Default theme id: warmer and higher contrast than ocean on dark terminals.
pub const DEFAULT_DIFF_THEME: &str = "eighties";

/// The bundled syntax definitions, loaded once per process.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// All bundled syntect themes, loaded once.
fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// Currently selected theme id (one of [`DIFF_THEMES`]'s short ids).
fn current_theme_id() -> &'static Mutex<String> {
    static ID: OnceLock<Mutex<String>> = OnceLock::new();
    ID.get_or_init(|| Mutex::new(DEFAULT_DIFF_THEME.to_string()))
}

/// Resolves a config value (short id or syntect name) to a known short id.
pub fn normalize_theme_id(raw: &str) -> &'static str {
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_DIFF_THEME;
    }
    for (id, _, syntect_name) in DIFF_THEMES {
        if raw.eq_ignore_ascii_case(id) || raw == *syntect_name {
            return id;
        }
    }
    DEFAULT_DIFF_THEME
}

/// Display label for a theme id (falls back to the default's label).
pub fn theme_label(id: &str) -> &'static str {
    let id = normalize_theme_id(id);
    DIFF_THEMES
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, label, _)| *label)
        .unwrap_or("Eighties")
}

/// Syntect theme name for a short id.
fn syntect_name_for(id: &str) -> &'static str {
    let id = normalize_theme_id(id);
    DIFF_THEMES
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, _, name)| *name)
        .unwrap_or("base16-eighties.dark")
}

/// The short id of the theme currently used for highlighting.
pub fn active_theme_id() -> String {
    current_theme_id()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| DEFAULT_DIFF_THEME.to_string())
}

/// Switches the diff highlighter to `raw` (a short id or syntect name) and
/// clears the render cache so the next frame uses the new colours.
pub fn set_theme(raw: &str) {
    let id = normalize_theme_id(raw).to_string();
    if let Ok(mut guard) = current_theme_id().lock() {
        if *guard == id {
            return;
        }
        *guard = id;
    }
    clear_cache();
}

/// Cycles to the next theme in [`DIFF_THEMES`], returning the new short id.
#[allow(dead_code)] // exercised by unit tests; available for future keybinds
pub fn cycle_theme() -> &'static str {
    let current = active_theme_id();
    let idx = DIFF_THEMES
        .iter()
        .position(|(id, _, _)| *id == current)
        .unwrap_or(0);
    let next = DIFF_THEMES[(idx + 1) % DIFF_THEMES.len()].0;
    set_theme(next);
    next
}

/// Looks up a syntect [`Theme`] by short id.
fn theme_by_id(id: &str) -> &'static Theme {
    let name = syntect_name_for(id);
    theme_set()
        .themes
        .get(name)
        .unwrap_or_else(|| &theme_set().themes["base16-eighties.dark"])
}

/// Looks up the active syntect [`Theme`].
fn theme() -> &'static Theme {
    // Themes are borrowed from the static ThemeSet; we re-resolve by id each
    // call so a settings change takes effect without restarting.
    theme_by_id(&active_theme_id())
}

/// Tiny unified-diff snippet shown under Settings while cycling `diff_theme`.
/// Short enough to fit the settings form column; `.rs` so syntect colours it.
pub const THEME_PREVIEW_SAMPLE: &str = "\
@@ -1,2 +1,2 @@
-fn greet(n: i32) { println!(\"{n}\"); }
+fn greet(n: i32) { println!(\"{n}!\"); }
";

/// Renders [`THEME_PREVIEW_SAMPLE`] with `theme_id`'s palette without touching
/// the process-wide active theme (so an unsaved Settings cycle stays a preview).
pub fn theme_preview_lines(theme_id: &str) -> Vec<Line<'static>> {
    render_with_theme(
        "preview.rs",
        THEME_PREVIEW_SAMPLE,
        theme_by_id(normalize_theme_id(theme_id)),
    )
}

thread_local! {
    /// One-entry render cache. Only one diff is ever on screen, and its text
    /// changes rarely (on file switch or refresh) compared to how often the
    /// frame redraws, so caching the last render avoids re-highlighting the
    /// same content on every tick. The key includes the theme id so a theme
    /// switch never serves a stale palette.
    static CACHE: RefCell<Option<(u64, Vec<Line<'static>>)>> = const { RefCell::new(None) };
}

/// Drops the cached highlighted lines (called when the theme changes).
fn clear_cache() {
    CACHE.with(|c| *c.borrow_mut() = None);
}

/// Renders unified diff text for `path` as styled lines: syntax-highlighted
/// code with green/red line backgrounds for additions/removals. Results are
/// memoized on (theme, path, content), so calling this every frame is cheap.
pub fn diff_lines(path: &str, content: &str) -> Vec<Line<'static>> {
    let mut hasher = DefaultHasher::new();
    active_theme_id().hash(&mut hasher);
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
    render_with_theme(path, content, theme())
}

/// Like [`render`], but uses an explicit syntect theme (for Settings previews).
fn render_with_theme(path: &str, content: &str, syn_theme: &Theme) -> Vec<Line<'static>> {
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
            Some(HighlightLines::new(s, syn_theme))
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
fn diff_line(line: &str, highlighter: &mut Option<HighlightLines<'_>>) -> Line<'static> {
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
            Style::new().add_modifier(Modifier::BOLD).fg(theme::INFO),
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
        // The header keeps its INFO-colored bold styling with no tint.
        assert_eq!(lines[0].style.fg, Some(theme::INFO));
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

    #[test]
    fn normalize_theme_id_accepts_short_and_syntect_names() {
        assert_eq!(normalize_theme_id(""), DEFAULT_DIFF_THEME);
        assert_eq!(normalize_theme_id("ocean"), "ocean");
        assert_eq!(normalize_theme_id("base16-ocean.dark"), "ocean");
        assert_eq!(normalize_theme_id("nope"), DEFAULT_DIFF_THEME);
    }

    #[test]
    fn cycle_theme_walks_the_catalog() {
        set_theme(DEFAULT_DIFF_THEME);
        let first = cycle_theme();
        assert_ne!(first, DEFAULT_DIFF_THEME);
        // Walk the rest of the way back so other tests see the default.
        for _ in 0..DIFF_THEMES.len() {
            if active_theme_id() == DEFAULT_DIFF_THEME {
                break;
            }
            cycle_theme();
        }
        set_theme(DEFAULT_DIFF_THEME);
    }

    #[test]
    fn theme_preview_does_not_mutate_the_active_theme() {
        set_theme("ocean");
        let before = active_theme_id();
        let lines = theme_preview_lines("mocha");
        assert_eq!(active_theme_id(), before, "preview must not call set_theme");
        assert_eq!(lines.len(), THEME_PREVIEW_SAMPLE.lines().count());
        // Sample includes an added and a removed line with the usual tints.
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.bg == Some(ADD_BG))),
            "expected a green-tinted addition: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.bg == Some(DEL_BG))),
            "expected a red-tinted removal: {lines:?}"
        );
        set_theme(DEFAULT_DIFF_THEME);
    }
}
