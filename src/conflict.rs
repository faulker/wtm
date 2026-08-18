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
///
/// Every variant keeps something, so the shared `Keep` prefix is the meaning
/// rather than noise, and dropping it would leave `Ours`/`Theirs`/`Both` reading
/// as sides instead of decisions.
#[allow(clippy::enum_variant_names)]
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

/// Appends one hunk resolved by `action` to `out`.
fn push_resolved(out: &mut String, ours: &str, theirs: &str, action: &ResolutionAction) {
    match action {
        ResolutionAction::KeepOurs => out.push_str(ours),
        ResolutionAction::KeepTheirs => out.push_str(theirs),
        ResolutionAction::KeepBoth => {
            out.push_str(ours);
            out.push_str(theirs);
        }
        ResolutionAction::KeepBothReversed => {
            out.push_str(theirs);
            out.push_str(ours);
        }
    }
}

/// Appends `text` to `out`, adding a newline first when `out` doesn't already
/// end in one. Conflict markers must start their own line, and a hunk side that
/// came from a file with no trailing newline (or from the manual editor) may
/// not end in one.
fn push_line_start(out: &mut String, text: &str) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
}

/// Appends one hunk to `out` still wrapped in its conflict markers, labelled
/// with `ours_label`/`theirs_label`. Used to leave an undecided hunk exactly as
/// git would have written it, so the file can be saved half-resolved and
/// re-parsed later.
fn push_unresolved(
    out: &mut String,
    ours: &str,
    theirs: &str,
    base: Option<&String>,
    ours_label: &str,
    theirs_label: &str,
) {
    push_line_start(out, &format!("<<<<<<< {ours_label}\n"));
    out.push_str(ours);
    if let Some(base) = base {
        push_line_start(out, "||||||| base\n");
        out.push_str(base);
    }
    push_line_start(out, "=======\n");
    out.push_str(theirs);
    push_line_start(out, &format!(">>>>>>> {theirs_label}\n"));
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
                    Some(action) => push_resolved(&mut out, ours, theirs, action),
                    None => out.push_str(ours),
                }
                hunk_index += 1;
            }
        }
    }
    out
}

/// Like [`render`], but for a file that is only partly resolved: a hunk with an
/// action is written out resolved, while a hunk still set to `None` is written
/// back with its conflict markers intact, labelled `ours_label`/`theirs_label`.
///
/// This is what lets the resolver save work in progress. The result is a valid
/// conflicted file, so `parse` round-trips it and git still sees the path as
/// unmerged until every hunk is decided.
pub fn render_partial(
    segments: &[ConflictSegment],
    resolutions: &[Option<ResolutionAction>],
    ours_label: &str,
    theirs_label: &str,
) -> String {
    let mut out = String::new();
    let mut hunk_index = 0;
    for segment in segments {
        match segment {
            ConflictSegment::Plain(text) => out.push_str(text),
            ConflictSegment::Hunk { ours, theirs, base } => {
                match resolutions.get(hunk_index).and_then(|a| a.as_ref()) {
                    Some(action) => push_resolved(&mut out, ours, theirs, action),
                    None => push_unresolved(
                        &mut out,
                        ours,
                        theirs,
                        base.as_ref(),
                        ours_label,
                        theirs_label,
                    ),
                }
                hunk_index += 1;
            }
        }
    }
    out
}

/// True when any hunk in `segments` is still unresolved, i.e. the file would
/// still be written with conflict markers.
pub fn has_conflicts(segments: &[ConflictSegment]) -> bool {
    segments
        .iter()
        .any(|s| matches!(s, ConflictSegment::Hunk { .. }))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-hunk conflicted file in git's default (non-diff3) format.
    const TWO_HUNKS: &str = "\
top
<<<<<<< HEAD
mine one
=======
yours one
>>>>>>> feature
middle
<<<<<<< HEAD
mine two
=======
yours two
>>>>>>> feature
bottom
";

    #[test]
    fn render_partial_keeps_undecided_hunks_as_markers() {
        let segments = parse(TWO_HUNKS);
        let out = render_partial(&segments, &[None, None], "HEAD", "feature");
        assert_eq!(out, TWO_HUNKS, "an all-undecided render round-trips");
        assert_eq!(parse(&out), segments, "and re-parses to the same segments");
    }

    #[test]
    fn render_partial_mixes_resolved_and_unresolved() {
        let segments = parse(TWO_HUNKS);
        let out = render_partial(
            &segments,
            &[Some(ResolutionAction::KeepTheirs), None],
            "HEAD",
            "feature",
        );
        assert!(out.contains("yours one\n"), "hunk 1 resolved: {out}");
        assert!(!out.contains("mine one"), "hunk 1 dropped ours: {out}");
        // Exactly the second hunk is left conflicted, so git still sees the
        // path as unmerged and the resolver can pick it up again.
        assert_eq!(out.matches("<<<<<<<").count(), 1, "{out}");
        assert!(
            out.contains("mine two") && out.contains("yours two"),
            "{out}"
        );
        let reparsed = parse(&out);
        assert_eq!(
            reparsed
                .iter()
                .filter(|s| matches!(s, ConflictSegment::Hunk { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn render_partial_fully_resolved_matches_render() {
        let segments = parse(TWO_HUNKS);
        let actions = [ResolutionAction::KeepOurs, ResolutionAction::KeepTheirs];
        let partial = render_partial(&segments, &actions.clone().map(Some), "HEAD", "feature");
        assert_eq!(partial, render(&segments, &actions));
        assert!(!has_conflicts(&parse(&partial)));
    }

    #[test]
    fn render_partial_preserves_the_diff3_base_section() {
        let text = "\
<<<<<<< HEAD
mine
||||||| merged common ancestors
original
=======
yours
>>>>>>> feature
";
        let segments = parse(text);
        let out = render_partial(&segments, &[None], "HEAD", "feature");
        assert!(out.contains("original\n"), "base section kept: {out}");
        assert_eq!(parse(&out), segments, "segments round-trip: {out}");
    }

    #[test]
    fn render_partial_inserts_a_newline_before_a_marker_when_a_side_lacks_one() {
        // A manual edit (or a file with no trailing newline) can leave a side
        // that doesn't end in "\n"; the following marker must still start its
        // own line or the file re-parses wrong.
        let segments = vec![ConflictSegment::Hunk {
            ours: "no trailing newline".to_string(),
            theirs: "theirs\n".to_string(),
            base: None,
        }];
        let out = render_partial(&segments, &[None], "HEAD", "feature");
        assert!(out.contains("no trailing newline\n=======\n"), "{out}");
        // Re-parsing normalizes that side to end in a newline. That is the
        // point: the content survives and the markers stay well-formed.
        assert_eq!(
            parse(&out),
            vec![ConflictSegment::Hunk {
                ours: "no trailing newline\n".to_string(),
                theirs: "theirs\n".to_string(),
                base: None,
            }],
            "{out}"
        );
    }

    #[test]
    fn has_conflicts_reports_remaining_hunks() {
        assert!(has_conflicts(&parse(TWO_HUNKS)));
        assert!(!has_conflicts(&parse("just plain text\n")));
    }
}
