//! Parsing and rendering of git's conflict-marker format inside a single file.
//!
//! A conflicted file mixes ordinary text with one or more conflict hunks
//! delimited by `<<<<<<<`, optionally `|||||||` (the diff3 common-ancestor
//! section), `=======`, and `>>>>>>>`. This module turns that text into a
//! structured [`Vec<ConflictSegment>`] and back, so callers can inspect or
//! resolve each hunk without re-parsing raw markers themselves.

use serde::Serialize;

/// One parsed unit of a conflicted file's contents, in file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ConflictSegment {
    /// A run of text with no unresolved conflict, verbatim.
    Plain(String),
    /// One conflict hunk: everything between a `<<<<<<<` marker and its
    /// matching `>>>>>>>`.
    Hunk {
        /// "Ours" side, between `<<<<<<<` and `|||||||`/`=======`.
        ours: String,
        /// "Theirs" side, between `=======` and `>>>>>>>`.
        theirs: String,
        /// Diff3 common-ancestor text (the `|||||||` section); `None` in the
        /// default 2-way conflict format.
        base: Option<String>,
    },
}

/// How to resolve a single conflict hunk when rendering a resolved file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionAction {
    /// Keep only "our" side.
    KeepOurs,
    /// Keep only "their" side.
    KeepTheirs,
    /// Keep both, ours then theirs.
    KeepBoth,
    /// Keep both, theirs then ours.
    KeepBothReversed,
    /// Replace the hunk with arbitrary text from the resolver's manual editor.
    Manual(String),
}

/// Splits `text` into lines, keeping each line's trailing `\n` (and any `\r`
/// immediately before it) attached, so re-joining every piece reproduces the
/// input exactly. The final line has no trailing newline when `text` doesn't
/// end in one.
fn lines_with_endings(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            lines.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// True when `line` is the `|||||||` (diff3 base) marker.
fn is_base_marker(line: &str) -> bool {
    line.starts_with("|||||||")
}

/// True when `line` is the `=======` (ours/theirs divider) marker. Compared
/// with the line ending stripped, since this marker carries no label.
fn is_divider_marker(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r']) == "======="
}

/// True when `line` is the `>>>>>>>` (end of hunk) marker.
fn is_end_marker(line: &str) -> bool {
    line.starts_with(">>>>>>>")
}

/// Parses conflict-marker text into ordered segments. Handles multiple hunks
/// interleaved with plain runs, and both the default 2-way format and diff3
/// (with a `|||||||` common-ancestor section).
pub fn parse(text: &str) -> Vec<ConflictSegment> {
    let lines = lines_with_endings(text);
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("<<<<<<<") {
            if !plain.is_empty() {
                segments.push(ConflictSegment::Plain(std::mem::take(&mut plain)));
            }
            i += 1; // skip <<<<<<< label

            let mut ours = String::new();
            while i < lines.len() && !is_base_marker(lines[i]) && !is_divider_marker(lines[i]) {
                ours.push_str(lines[i]);
                i += 1;
            }

            let mut base = None;
            if i < lines.len() && is_base_marker(lines[i]) {
                i += 1; // skip ||||||| label
                let mut base_text = String::new();
                while i < lines.len() && !is_divider_marker(lines[i]) {
                    base_text.push_str(lines[i]);
                    i += 1;
                }
                base = Some(base_text);
            }
            if i < lines.len() && is_divider_marker(lines[i]) {
                i += 1; // skip =======
            }

            let mut theirs = String::new();
            while i < lines.len() && !is_end_marker(lines[i]) {
                theirs.push_str(lines[i]);
                i += 1;
            }
            if i < lines.len() && is_end_marker(lines[i]) {
                i += 1; // skip >>>>>>> label
            }

            segments.push(ConflictSegment::Hunk { ours, theirs, base });
        } else {
            plain.push_str(lines[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        segments.push(ConflictSegment::Plain(plain));
    }
    segments
}

/// Renders resolved file text from parsed `segments`, applying `resolutions`
/// to the hunks in order (one action per hunk). A hunk with no matching entry
/// falls back to keeping "ours".
pub fn render(segments: &[ConflictSegment], resolutions: &[ResolutionAction]) -> String {
    let mut out = String::new();
    let mut hunk_index = 0;
    for segment in segments {
        match segment {
            ConflictSegment::Plain(text) => out.push_str(text),
            ConflictSegment::Hunk { ours, theirs, .. } => {
                match resolutions.get(hunk_index) {
                    Some(ResolutionAction::KeepOurs) | None => out.push_str(ours),
                    Some(ResolutionAction::KeepTheirs) => out.push_str(theirs),
                    Some(ResolutionAction::KeepBoth) => {
                        out.push_str(ours);
                        out.push_str(theirs);
                    }
                    Some(ResolutionAction::KeepBothReversed) => {
                        out.push_str(theirs);
                        out.push_str(ours);
                    }
                    Some(ResolutionAction::Manual(text)) => out.push_str(text),
                }
                hunk_index += 1;
            }
        }
    }
    out
}

/// Extracts the `ours`/`theirs` labels git wrote on the conflict markers (the
/// text after `<<<<<<< ` and `>>>>>>> `), when present. Git repeats the same
/// labels on every hunk in a file, so the first occurrence of each is enough.
pub fn marker_labels(text: &str) -> (Option<String>, Option<String>) {
    let lines = lines_with_endings(text);
    let ours = lines
        .iter()
        .find(|l| l.starts_with("<<<<<<<"))
        .map(|l| l.trim_start_matches("<<<<<<<").trim().to_string())
        .filter(|s| !s.is_empty());
    let theirs = lines
        .iter()
        .find(|l| l.starts_with(">>>>>>>"))
        .map(|l| l.trim_start_matches(">>>>>>>").trim().to_string())
        .filter(|s| !s.is_empty());
    (ours, theirs)
}
