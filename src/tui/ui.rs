//! TUI rendering: worktree list, diff viewer, and dialogs.
//!
//! Visual language: rounded panels with dim borders, one accent color for
//! titles/keys/selection, and a footer that always shows the active keys.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
};

use super::app::{
    App, BranchRow, CheckoutCandidate, CherryTarget, CommitFocus, ConfirmOption, CopyHit,
    CreateOutcome, DiffRow, LogMode, Modal, ResolverFile, ResolverHits, RowList, Tab, TextInput,
    UpstreamRow, View, WorktreesFocus, branch_display_rows, branch_row_of, filtered_candidates,
    upstream_rows,
};
use super::config_editor::{
    BRANCHES_REFRESH_ROW, CHECK_ROW, CONFLICT_EDITOR_ROW, COPY_ROW, ConfigEditor,
    DIFF_LINE_NUMBERS_ROW, FIELD_ROWS, LAYOUT_ROW, OPEN_COMMAND_ROW, OpenCommandEditor, RUN_ROW,
    StringListEditor, THEME_PREVIEW_SAMPLE_LINES, THEME_ROW, UPDATE_ROW, check_line, form_lines,
    line_of_row, preview_line,
};
use super::help::{self, Binding, HelpTab};
use super::highlight;
use super::setup::{
    REVIEW_ROWS, SetupWizard, Step, WELCOME_OPTIONS, location_label, location_preview,
};
use super::theme::{self, ACCENT, BORDER, DIALOG_BG, DIALOG_BORDER, GRAPH_COLORS, SELECTION_BG};
use crate::config::{
    CommandMode, DEFAULT_AUTO_UPDATE_CHECK, DEFAULT_BRANCHES_REFRESH_MINS,
    DEFAULT_DIFF_LINE_NUMBERS, DEFAULT_DIFF_THEME, DEFAULT_LOCATION, LOCATION_PRESETS, OpenCommand,
    OpenCommandVars, WorktreesLayout, expand_open_command, worktrees_layout_label,
};
use crate::conflict::{ConflictSegment, ResolutionAction};
use crate::git::{GraphLine, StatusEntry};
use crate::ops::{BranchListItem, ResolveKind};
use crate::update::{CURRENT_VERSION, Release};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Click targets are re-recorded from scratch each frame by whoever draws
    // them, so last frame's geometry can't outlive what's on screen. Cleared
    // before the header draws, since the header records one of them.
    app.copy_hits.clear();
    draw_header(frame, header, app);
    app.tab_hits.clear();
    app.preview_list = None;
    app.files_list = None;
    app.diff_path_hit = None;
    app.resolver_hits = None;
    // The full-screen view's clickable list, if any.
    let list_hit = match &app.view {
        View::Log {
            name,
            lines,
            own,
            selected,
        } => draw_log(frame, main, name, lines, own, *selected, app.log_mode),
        View::CommitDiff {
            label,
            rows,
            files,
            selected,
            content,
            loading_new,
            scroll,
            h_scroll,
            ..
        } => draw_commit_diff(
            frame,
            main,
            label,
            files,
            rows,
            *selected,
            content,
            *loading_new,
            *scroll,
            *h_scroll,
            app.ctx.config.diff_line_numbers(),
        ),
        View::StashDiff {
            label,
            rows,
            files,
            selected,
            content,
            loading_new,
            scroll,
            h_scroll,
            ..
        } => draw_commit_diff(
            frame,
            main,
            label,
            files,
            rows,
            *selected,
            content,
            *loading_new,
            *scroll,
            *h_scroll,
            app.ctx.config.diff_line_numbers(),
        ),
        View::BranchCommits {
            branch,
            lines,
            marked,
            own,
            selected,
        } => draw_branch_commits(
            frame,
            main,
            branch,
            lines,
            marked,
            own,
            *selected,
            app.log_mode,
        ),
        // The first-run setup wizard takes over the whole main area (there is no
        // repo state to show behind it); drawn in the overlay match below.
        View::Setup(_) => None,
        // Everything else renders the home tabs (worktrees or branches) as the
        // backdrop, with the tab bar on top; floating overlays draw over it.
        _ => {
            let [bar, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main);
            // The layout in force for this frame: the configured one, unless
            // the terminal is too short to give three panels a usable height,
            // in which case the tab falls back to two (and the Changes tab
            // comes back, so the diff is still reachable).
            app.three_panel = app.ctx.config.worktrees_layout() == WorktreesLayout::ThreePanel
                && body.height >= THREE_PANEL_MIN_HEIGHT;
            draw_tab_bar(frame, bar, app);
            match app.tab {
                Tab::Worktrees => draw_worktrees_tab(frame, body, app),
                Tab::Branches => draw_branches(frame, body, app),
                // A conflicted worktree has no ordinary changes view: the
                // resolver stands in for the file list and diff until every
                // conflict is settled, then this pane hands itself back.
                Tab::Changes if app.resolver.is_some() => {
                    let out = {
                        let r = app.resolver.as_ref().expect("checked just above");
                        draw_conflict_resolver(
                            frame,
                            body,
                            &r.target,
                            &r.source_label,
                            &r.kind,
                            &r.files,
                            &r.resolved,
                            r.file,
                            r.current.as_ref(),
                            r.loading.as_deref(),
                            app.resolver_scroll,
                            app.resolver_follow,
                            true,
                        )
                    };
                    app.apply_resolver_draw(out.scroll, out.max_scroll, out.page, out.hits);
                    out.list
                }
                Tab::Changes => draw_diff(
                    frame,
                    body,
                    &app.changes.name,
                    &app.changes.files,
                    &app.changes.marked,
                    &app.changes.rows,
                    app.changes.selected,
                    &app.changes.content,
                    app.changes.loading_new,
                    app.changes.scroll,
                    app.changes.h_scroll,
                    true,
                    &mut app.diff_path_hit,
                    app.ctx.config.diff_line_numbers(),
                ),
                Tab::Stash => draw_stash_tab(frame, body, app),
                Tab::Settings => {
                    draw_settings_tab(frame, body, &app.settings, app.update_available.as_ref())
                }
            }
        }
    };
    draw_footer(frame, footer, app);

    // Overlays on top of the list. An overlay with its own selectable list
    // reports it here so clicks land on the overlay, not the list beneath it.
    let mut overlay_hit = None;
    match &app.view {
        View::Create {
            name,
            branches,
            all_branches,
            base,
            selected,
            base_focus,
            base_pick,
        } => draw_create_dialog(
            frame,
            main,
            name,
            branches,
            all_branches,
            base,
            *selected,
            *base_focus,
            *base_pick,
            app.worktree_base.as_deref(),
        ),
        View::Creating {
            branch,
            lines,
            outcome,
            input,
            kill_armed,
            ..
        } => draw_creating(
            frame,
            main,
            branch,
            lines,
            outcome.as_ref(),
            input,
            *kill_armed,
        ),
        View::Setup(wizard) => overlay_hit = draw_setup(frame, main, wizard),
        View::Commit {
            name,
            files,
            marked,
            cursor,
            input,
            body,
            focus,
            loading,
            ..
        } => {
            overlay_hit = draw_commit(
                frame, main, name, files, marked, *cursor, input, body, focus, *loading,
            )
        }
        View::Switch {
            name,
            branches,
            filter,
            selected,
        } => overlay_hit = draw_switch(frame, main, name, branches, filter, *selected),
        View::Busy { label, .. } => draw_busy(frame, main, label, app.tick_count),
        View::RunCommand { name, input, .. } => draw_run_command(frame, main, name, input),
        View::RenameWorktree { name, input } => draw_rename_worktree(frame, main, name, input),
        View::CherryPick {
            source_branch,
            summaries,
            targets,
            selected,
            mode,
            ..
        } => {
            overlay_hit = draw_cherry_pick(
                frame,
                main,
                source_branch,
                summaries,
                targets,
                *selected,
                *mode,
            )
        }
        View::MergePick {
            source_branch,
            targets,
            selected,
        } => overlay_hit = draw_merge_pick(frame, main, source_branch, targets, *selected),
        View::RebasePick {
            onto_branch,
            targets,
            selected,
        } => overlay_hit = draw_rebase_pick(frame, main, onto_branch, targets, *selected),
        View::MoveChanges {
            from,
            targets,
            selected,
        } => overlay_hit = draw_move_changes_pick(frame, main, from, targets, *selected),
        View::OpenCommand {
            name,
            path,
            branch,
            status,
            commands,
            selected,
        } => {
            overlay_hit = draw_open_command_pick(
                frame,
                main,
                name,
                &OpenCommandVars {
                    path,
                    name,
                    branch,
                    status,
                },
                commands,
                *selected,
            )
        }
        View::UpstreamPick {
            branch,
            current,
            candidates,
            filter,
            selected,
        } => {
            overlay_hit = draw_upstream_pick(
                frame,
                main,
                branch,
                current.as_deref(),
                candidates,
                filter,
                *selected,
            )
        }
        View::StashTarget {
            pop,
            label,
            targets,
            selected,
            ..
        } => overlay_hit = draw_stash_target_pick(frame, main, *pop, label, targets, *selected),
        _ => {}
    }

    // A modal overlay (confirm/prompt/hunk editor) floats over the active view.
    // Only Confirm reports its own rows; Prompt/FileEditor have none.
    let modal_hit = app
        .modal
        .is_some()
        .then(|| draw_modal(frame, main, app))
        .flatten();

    // The help overlay sits on top of whatever view is active, so `?` works
    // everywhere and returns to where it was opened.
    if app.show_help {
        draw_help(frame, main, app);
    }

    // Clicks go to the topmost selectable list: an overlay's own list when one
    // is up, otherwise the full-screen list for views that respond to clicks.
    // Other overlays cover the list, so clicks are disabled while they're up.
    app.row_list = match &app.view {
        View::List
        | View::CommitDiff { .. }
        | View::StashDiff { .. }
        | View::Log { .. }
        | View::BranchCommits { .. } => list_hit,
        View::Commit { .. }
        | View::Switch { .. }
        | View::CherryPick { .. }
        | View::MergePick { .. }
        | View::RebasePick { .. }
        | View::MoveChanges { .. }
        | View::OpenCommand { .. }
        | View::UpstreamPick { .. }
        | View::StashTarget { .. }
        | View::Setup(_) => overlay_hit,
        _ => None,
    };
    // A modal covers the list, so clicks go to it instead when it has its own
    // rows (Confirm); other modal kinds (Prompt, FileEditor) just suppress
    // clicks on whatever is behind them.
    if let Some(modal) = &app.modal {
        app.row_list = matches!(modal, Modal::Confirm { .. })
            .then_some(modal_hit)
            .flatten();
    }

    // The tab bar, the worktree preview, and the changed-file panel and its
    // clickable diff path only take clicks when the home view is what the user
    // is actually looking at: a dialog, overlay, or modal floats over them all,
    // and clicks belong to whatever is on top.
    if !matches!(app.view, View::List) || app.modal.is_some() {
        app.tab_hits.clear();
        app.preview_list = None;
        app.files_list = None;
        app.diff_path_hit = None;
        // The header keeps drawing under an overlay, so its path hit (and every
        // other copy region) has to be taken back here rather than skipped at
        // the point it was recorded.
        app.copy_hits.clear();
    }
    // The resolver's hunk pane records its geometry as it draws, but a modal
    // (the whole-file editor above all) covers it: without this, a click meant
    // for the editor lands on a hunk behind it and takes a side.
    if app.modal.is_some() {
        app.resolver_hits = None;
    }

    // The error popup sits on top of absolutely everything, including the
    // help overlay, and suppresses clicks on whatever is behind it. Cloned so
    // drawing it doesn't hold an immutable borrow while `row_list` is reset.
    if let Some(err) = app.error.clone() {
        app.error_max_scroll = draw_error_popup(frame, main, &err, app.error_scroll);
        app.error_scroll = app.error_scroll.min(app.error_max_scroll);
        app.row_list = None;
        app.tab_hits.clear();
        app.preview_list = None;
    }
}

/// A rounded panel with an accent-colored title and inner padding.
fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title.into(),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
}

/// Overlay chrome for dialogs and modals: a solid background and a thick
/// border so the popup is unmistakable over the screen underneath.
fn dialog_panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(DIALOG_BORDER))
        .style(Style::new().bg(DIALOG_BG))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title.into(),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
}

/// Panel chrome for a focusable list: accent border and bold title when it
/// owns the keyboard, dim border and title when another panel does. Diff
/// panes and other non-focus targets keep using [`panel`].
fn focus_panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    let (border, title_style) = if focused {
        (ACCENT, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
    } else {
        (BORDER, Style::new().fg(BORDER))
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.into(), title_style),
            Span::raw(" "),
        ]))
}

/// Top bar: app badge and the selected worktree's path on the left; the
/// worktree count on the right, or the transient status/error message when one
/// is present. Falls back to the repo root when nothing is selected.
fn draw_header(frame: &mut Frame, area: Rect, app: &mut App) {
    let count = app.worktrees.len();
    let path = app
        .worktrees
        .get(app.selected)
        .map(|wt| wt.path.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| app.ctx.repo_root.display().to_string());
    // The badge plus the two spaces after it, so the path's own rect can be
    // recorded for a click-to-copy without re-measuring the badge.
    const BADGE_W: u16 = 7;
    if !path.is_empty() {
        app.copy_hits.push(CopyHit {
            rect: Rect {
                x: area.x + BADGE_W,
                y: area.y,
                width: (path.chars().count() as u16).min(area.width.saturating_sub(BADGE_W)),
                height: 1,
            },
            value: path.clone(),
            label: "path",
            row: None,
        });
    }
    let left = Line::from(vec![
        Span::styled(" wtm ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
        Span::styled(path, Style::new().bold()),
    ]);
    // The right slot is wide enough for the message (or count), and is drawn
    // right-aligned so it never overlaps the app badge.
    let right = match &app.message {
        // Errors now show as a modal popup (see `draw_error_popup`), so every
        // message reaching the header is a plain status/info line.
        Some(msg) => Line::styled(format!("{msg} "), Style::new().fg(theme::WARNING).bold()),
        None => Line::styled(
            format!("({count} worktree{}) ", if count == 1 { "" } else { "s" }),
            Style::new().dim(),
        ),
    };
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// Shortens `text` to at most `max` characters by eliding its middle with `…`.
/// Both ends of a branch name (and of a file path) carry meaning (the namespace
/// or directory prefix and the distinguishing tail), so trimming from the middle
/// keeps more signal than a trailing ellipsis would. Counts characters, not bytes, so a multi-byte name
/// can't be split mid-codepoint.
pub(super) fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    // Below three characters there is no room for both ends plus the ellipsis.
    if max <= 1 {
        return "…".to_string();
    }
    // Bias the extra character to the front when the budget is odd: the prefix
    // is usually the namespace and reads as the more identifying half.
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Shortens `text` to at most `max` characters by dropping characters off the
/// front and marking the cut with a leading `…`. Filesystem paths are
/// identified by their tail (the worktree directory), so the shared prefix is
/// what gives way. Counts characters, not bytes.
pub(super) fn truncate_start(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    // Below two characters there is no room for the ellipsis plus any tail.
    if max <= 1 {
        return if max == 0 {
            String::new()
        } else {
            "…".to_string()
        };
    }
    let keep = max - 1;
    let mut out = String::from("…");
    out.extend(&chars[chars.len() - keep..]);
    out
}

/// Footer as key hints: the key in accent, its label dimmed. Bindings with no
/// `short` label are help-panel-only and skipped here. Trailing hints that
/// would overflow `width` are dropped and replaced with `…` so a narrow
/// terminal never bleeds past the edge.
fn hint_line_fitting(bindings: &[Binding], width: Option<u16>) -> Line<'static> {
    let max = width.map(|w| w as usize);
    let mut spans = Vec::new();
    let mut used = 0usize;
    let items: Vec<_> = bindings
        .iter()
        .filter_map(|b| b.short.map(|s| (b.key, s)))
        .collect();
    for (i, (key, label)) in items.iter().enumerate() {
        let sep = if spans.is_empty() { 0 } else { 2 };
        let piece = key.chars().count() + 1 + label.chars().count();
        if let Some(max) = max {
            // Reserve room for a trailing "  …" when more hints remain.
            let ellipsis = if i + 1 < items.len() { 3 } else { 0 };
            if used + sep + piece + ellipsis > max {
                if !spans.is_empty() {
                    spans.push(Span::styled("  …", Style::new().dim()));
                }
                break;
            }
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {label}"), Style::new().dim()));
        used += sep + piece;
    }
    Line::from(spans)
}

/// Shorthand for the footer-only hints of views that have no help section.
const fn hint(key: &'static str, label: &'static str) -> Binding {
    Binding {
        key,
        short: Some(label),
        long: label,
    }
}

/// Height the three-panel Worktrees layout gives the worktree list: four rows
/// plus the table header and the panel's borders.
const THREE_PANEL_LIST_HEIGHT: u16 = 7;

/// Shortest body the three-panel layout is drawn in: the worktree list plus
/// enough room for a file list and diff worth reading. Below this the tab falls
/// back to two panels.
const THREE_PANEL_MIN_HEIGHT: u16 = THREE_PANEL_LIST_HEIGHT + 7;

/// Row highlight for a selectable list, dimmed when the panel doesn't hold the
/// keyboard so only one cursor on screen looks live.
fn highlight_style(focused: bool) -> Style {
    if focused {
        Style::new().bg(SELECTION_BG).bold()
    } else {
        Style::new().bg(SELECTION_BG).dim()
    }
}

/// Color of a list's cursor marker, matching `highlight_style`.
fn cursor_color(focused: bool) -> Color {
    if focused { ACCENT } else { BORDER }
}

/// Colored FLAGS spans from the shared `flag_labels` vocabulary.
fn status_flag_spans(labels: &[&str]) -> Vec<Span<'static>> {
    if labels.is_empty() {
        return vec![Span::styled("–".to_string(), Style::new().dim())];
    }
    let mut spans = Vec::new();
    for label in labels {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        let (text, style) = match *label {
            "unpushed" => ("unpushed", Style::new().fg(theme::WARNING)),
            // The reassuring counterpart of `unpushed`: the work is on the
            // remote, so it reads calm rather than as something to act on.
            "pushed" => ("pushed", Style::new().fg(theme::SUCCESS)),
            "behind" => ("behind", Style::new().fg(theme::INFO)),
            "changed" => ("changed", Style::new().fg(theme::WARNING)),
            "same" => ("same", Style::new().dim()),
            "outdated" => ("outdated", Style::new().fg(theme::WARNING)),
            "merged" => ("✓merged", Style::new().fg(theme::SUCCESS)),
            "locked" => ("locked", Style::new().fg(theme::DANGER)),
            other => (other, Style::new()),
        };
        spans.push(Span::styled(text.to_string(), style));
    }
    spans
}

/// `"s"` when `n` is not 1, for pluralising a count inline.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Present-tense label for an interrupted operation, as shown in the worktree
/// list's flags column.
fn in_progress_label(kind: &ResolveKind) -> &'static str {
    match kind {
        ResolveKind::Merge => "merging",
        ResolveKind::Rebase => "rebasing",
        ResolveKind::CherryPick => "cherry-picking",
        ResolveKind::Revert => "reverting",
        ResolveKind::StashPop { .. } => "unstashing",
    }
}

/// Fixed column widths of the worktree table, shared by the layout constraints
/// and by the path-truncation budget derived from them.
const CHANGES_W: u16 = 14;
const UPSTREAM_W: u16 = 9;
const FLAGS_W: u16 = 30;
const MIN_PATH_W: u16 = 20;
const COLUMN_SPACING: u16 = 1;
/// Width of the row cursor drawn by `highlight_symbol` ("▌ ").
const HIGHLIGHT_SYMBOL_W: u16 = 2;

/// The worktree table. `focused` is false when another panel owns the keyboard
/// (the three-panel layout's file list), which dims the row highlight so it is
/// clear which cursor the arrow keys move.
fn draw_list(frame: &mut Frame, area: Rect, app: &mut App, focused: bool) -> Option<RowList> {
    let name_w = app
        .worktrees
        .iter()
        .map(|w| w.name.len() + 2)
        .max()
        .unwrap_or(10)
        .max(10) as u16;
    let block = focus_panel("worktrees", focused);
    let inner = block.inner(area);
    // Mirror the table's own horizontal layout so the path can be trimmed from
    // the front before ratatui would clip it from the back: the highlight
    // symbol column, the four fixed columns, and one space between all five.
    let path_w = inner.width.saturating_sub(
        HIGHLIGHT_SYMBOL_W + name_w + CHANGES_W + UPSTREAM_W + FLAGS_W + COLUMN_SPACING * 4,
    ) as usize;
    let rows: Vec<Row> = app
        .worktrees
        .iter()
        .map(|wt| {
            let name = Line::from(vec![
                Span::styled(wt.name.clone(), Style::new().bold()),
                if wt.is_main {
                    Span::styled(" ●", Style::new().fg(ACCENT))
                } else {
                    Span::raw("")
                },
            ]);
            // An unmerged file outranks the plain dirty count: it means the
            // worktree is stopped mid-operation and needs attention, not just
            // that it has edits.
            let changes = if wt.conflicted > 0 {
                Span::styled(
                    format!("⚠ {} conflict{}", wt.conflicted, plural(wt.conflicted)),
                    Style::new().fg(theme::DANGER).bold(),
                )
            } else if wt.dirty > 0 {
                Span::styled(
                    format!("{} changed", wt.dirty),
                    Style::new().fg(theme::WARNING),
                )
            } else {
                Span::styled("clean".to_string(), Style::new().fg(theme::SUCCESS))
            };
            let upstream = match wt.ahead_behind {
                Some(ab) => Span::styled(
                    format!("↑{} ↓{}", ab.ahead, ab.behind),
                    Style::new().fg(ACCENT),
                ),
                None => Span::styled("–".to_string(), Style::new().dim()),
            };
            // Flags: the interrupted operation first (it decides what the user
            // can do next), then upstream sync, base-relative status, and
            // cleanup/lock.
            let mut flag_spans = Vec::new();
            if let Some(kind) = &wt.in_progress {
                flag_spans.push(Span::styled(
                    in_progress_label(kind).to_string(),
                    Style::new().fg(theme::DANGER).bold(),
                ));
                flag_spans.push(Span::raw(" "));
            }
            flag_spans.extend(status_flag_spans(&wt.flag_labels()));
            Row::new(vec![
                Cell::from(name),
                Cell::from(changes),
                Cell::from(upstream),
                Cell::from(Line::from(flag_spans)),
                Cell::from(Span::styled(
                    truncate_start(&wt.path, path_w),
                    Style::new().dim(),
                )),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(name_w),
            Constraint::Length(CHANGES_W),
            Constraint::Length(UPSTREAM_W),
            Constraint::Length(FLAGS_W),
            Constraint::Min(MIN_PATH_W),
        ],
    )
    .column_spacing(COLUMN_SPACING)
    .header(
        Row::new(["NAME", "CHANGES", "UPSTREAM", "FLAGS", "PATH"]).style(Style::new().dim().bold()),
    )
    .block(block)
    .row_highlight_style(highlight_style(focused))
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(cursor_color(focused))));
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
    let offset = state.offset();
    // Header takes the first inner row; remaining rows are the visible window.
    let visible = inner.height.saturating_sub(1) as usize;
    let total = app.worktrees.len();
    draw_scroll_arrows(frame, area, offset, visible, total);
    // Each visible PATH cell is click-to-copy, and copies the whole path even
    // where the cell shows a front-trimmed form of it. The hit is only as wide
    // as the text actually drawn, so the empty run to the right of a short path
    // stays an ordinary row click.
    let path_x = inner.x
        + HIGHLIGHT_SYMBOL_W
        + name_w
        + CHANGES_W
        + UPSTREAM_W
        + FLAGS_W
        + COLUMN_SPACING * 4;
    // Room left for the column, so a terminal too narrow to reach it records no
    // hits at all rather than a rect hanging off the panel.
    let path_room = (inner.x + inner.width).saturating_sub(path_x);
    for (i, wt) in app.worktrees.iter().enumerate().skip(offset).take(visible) {
        let drawn = truncate_start(&wt.path, path_w).chars().count() as u16;
        let width = drawn.min(path_room);
        if width == 0 {
            continue;
        }
        app.copy_hits.push(CopyHit {
            rect: Rect {
                x: path_x,
                y: inner.y + 1 + (i - offset) as u16,
                width,
                height: 1,
            },
            value: wt.path.clone(),
            label: "path",
            row: Some(i),
        });
    }
    // The table header occupies the first inner row, so data rows start one
    // line below it.
    Some(RowList {
        inner,
        header: 1,
        offset,
        len: total,
    })
}

/// Overflow arrows drawn in a panel's left border gutter: UP when rows sit
/// above the viewport, DOWN when more sit below. Most visible in the
/// three-panel Worktrees list, which is only a few rows tall. `visible` is the
/// number of data rows on screen, so a caller with a table header subtracts it
/// first; `area` is the panel rect including its border, not the inner rect.
fn draw_scroll_arrows(frame: &mut Frame, area: Rect, offset: usize, visible: usize, total: usize) {
    if offset > 0 {
        let arrow = Rect {
            x: area.x,
            y: area.y + 1,
            width: 1,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled("▲", Style::new().fg(ACCENT).bold())),
            arrow,
        );
    }
    if visible > 0 && offset + visible < total {
        let arrow = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: 1,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled("▼", Style::new().fg(ACCENT).bold())),
            arrow,
        );
    }
}

/// Worktrees tab: the worktree table on top, a read-only changed-file
/// preview for the highlighted row on the bottom.
fn draw_worktrees_tab(frame: &mut Frame, area: Rect, app: &mut App) -> Option<RowList> {
    if app.three_panel {
        return draw_worktrees_three_panel(frame, area, app);
    }
    let [list_area, preview_area] =
        Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
    let row_list = draw_list(frame, list_area, app, true);
    // Status runs off-thread; never call `ops::status` on the render path.
    app.ensure_worktree_preview();
    draw_worktree_preview(frame, preview_area, app);
    row_list
}

/// Three-panel Worktrees tab: a compact scrollable worktree list on top, with
/// the Changes tab's changed-file list and diff filling the space below it, so
/// the highlighted worktree's changes are readable without leaving the tab.
fn draw_worktrees_three_panel(frame: &mut Frame, area: Rect, app: &mut App) -> Option<RowList> {
    let [list_area, changes_area] = Layout::vertical([
        Constraint::Length(THREE_PANEL_LIST_HEIGHT),
        Constraint::Min(1),
    ])
    .areas(area);
    let focus = app.worktrees_focus;
    let row_list = draw_list(frame, list_area, app, focus == WorktreesFocus::List);
    // Status runs off-thread; never call `ops::status` on the render path. Its
    // result also swaps the two panels below (`sync_three_panel_changes`).
    app.ensure_worktree_preview();
    let files_focused = focus == WorktreesFocus::Files;
    if let Some(r) = &app.resolver {
        // The resolver replaces both bottom panels, the same way it replaces the
        // Changes tab's body: same two-pane geometry, conflicted files on the
        // left instead of changed ones.
        app.diff_path_hit = None;
        let out = draw_conflict_resolver(
            frame,
            changes_area,
            &r.target,
            &r.source_label,
            &r.kind,
            &r.files,
            &r.resolved,
            r.file,
            r.current.as_ref(),
            r.loading.as_deref(),
            app.resolver_scroll,
            app.resolver_follow,
            files_focused,
        );
        app.files_list = out.list;
        app.apply_resolver_draw(out.scroll, out.max_scroll, out.page, out.hits);
    } else if let Some(panel) = &app.worktree_commits {
        app.diff_path_hit = None;
        app.files_list = draw_worktree_commits(
            frame,
            changes_area,
            &panel.branch,
            &panel.lines,
            &panel.own,
            panel.selected,
            app.log_mode,
            files_focused,
            &mut app.copy_hits,
        );
    } else if app.commits_loading() {
        app.diff_path_hit = None;
        app.files_list = None;
        let title = match app.worktrees.get(app.selected) {
            Some(wt) => format!(
                "commits · {}",
                wt.branch.as_deref().unwrap_or(wt.name.as_str())
            ),
            None => "commits".to_string(),
        };
        let para = Paragraph::new(Line::from("loading…".dim())).block(panel(title));
        frame.render_widget(para, changes_area);
    } else {
        app.files_list = draw_diff(
            frame,
            changes_area,
            &app.changes.name,
            &app.changes.files,
            &app.changes.marked,
            &app.changes.rows,
            app.changes.selected,
            &app.changes.content,
            app.changes.loading_new,
            app.changes.scroll,
            app.changes.h_scroll,
            files_focused,
            &mut app.diff_path_hit,
            app.ctx.config.diff_line_numbers(),
        );
    }
    row_list
}

/// Read-only list of the highlighted worktree's changed files (status code and
/// path). Every changed file gets a row; when there are more than the panel is
/// tall the list scrolls (wheel or Shift+↑/↓) instead of being truncated. No
/// cursor or mark-for-commit state: the worktree table above still owns the
/// keyboard, but a click on a row opens that file on the Changes tab.
fn draw_worktree_preview(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = match app.worktrees.get(app.selected) {
        Some(wt) => format!("changes · {}", wt.name),
        None => "changes".to_string(),
    };
    // Selection moved (or refresh invalidated the cache) and status is still
    // in flight: show a placeholder rather than another worktree's files.
    if app.preview_loading() {
        let para = Paragraph::new(Line::from("loading…".dim())).block(panel(title));
        frame.render_widget(para, area);
        app.preview_list = None;
        return;
    }
    if app.worktree_preview.is_empty() {
        let para = Paragraph::new(Line::from("no changes".dim())).block(panel(title));
        frame.render_widget(para, area);
        app.preview_scroll = 0;
        app.preview_list = None;
        return;
    }

    let block = panel(title);
    let inner = block.inner(area);
    let total = app.worktree_preview.len();
    let visible = inner.height as usize;
    // Keep the viewport on the list: it can be left past the end by a shrinking
    // status refresh or by holding Shift+↓ (which doesn't know the height).
    app.preview_scroll = app.preview_scroll.min(total.saturating_sub(visible));
    let offset = app.preview_scroll;
    let lines: Vec<Line> = app
        .worktree_preview
        .iter()
        .skip(offset)
        .take(visible)
        .map(|entry| {
            Line::from(vec![
                Span::styled(format!("{:<3}", entry.code), status_style(&entry.code)),
                Span::raw(entry.path.clone()),
            ])
        })
        .collect();
    // Position readout in the border, so the panel says how much is off-screen
    // without spending a row on it.
    let block = if total > visible && visible > 0 {
        block.title_bottom(
            Line::from(format!(
                " {}-{}/{total} ",
                offset + 1,
                (offset + visible).min(total)
            ))
            .right_aligned()
            .dim(),
        )
    } else {
        block
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
    app.preview_list = (visible > 0).then_some(RowList {
        inner,
        header: 0,
        offset,
        len: total,
    });
}

/// The highlighted diff for `path`, with a line-number gutter down the left
/// when `numbered`. Numbers come from the diff's own hunk headers, so a removed
/// line shows its position in the pre-image; those are dimmed to say so. The
/// gutter is as narrow as the largest number allows, so a short file doesn't
/// pay for a five-digit column.
fn diff_lines_with_gutter(path: &str, content: &str, numbered: bool) -> Vec<Line<'static>> {
    let lines = highlight::diff_lines(path, content);
    if !numbered {
        return lines;
    }
    let numbers = highlight::gutter_numbers(content);
    let width = numbers
        .iter()
        .filter_map(|n| n.map(|(num, _)| num))
        .max()
        .map(|max| max.to_string().len())
        .unwrap_or(0);
    // No numbers at all (a binary or status-only diff): skip the empty gutter.
    if width == 0 {
        return lines;
    }
    lines
        .into_iter()
        .zip(numbers)
        .map(|(line, number)| {
            let (text, style) = match number {
                Some((num, highlight::GutterSide::New)) => {
                    (format!("{num:>width$} "), Style::new().fg(BORDER))
                }
                // A removed line's number points at the old file, so it is
                // dimmed further to distinguish it from a live line number.
                Some((num, highlight::GutterSide::Old)) => {
                    (format!("{num:>width$} "), Style::new().fg(BORDER).dim())
                }
                None => (" ".repeat(width + 1), Style::new()),
            };
            let mut spans = vec![Span::styled(text, style)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Path of the changed file under the cursor row, or "" on a folder row.
fn current_diff_path<'a>(rows: &[DiffRow], files: &'a [StatusEntry], selected: usize) -> &'a str {
    super::app::current_file_index(rows, selected)
        .and_then(|i| files.get(i))
        .map(|f| f.path.as_str())
        .unwrap_or("")
}

/// The per-file changes view: a folder tree of changed files on the left and
/// the highlighted file's diff on the right.
#[allow(clippy::too_many_arguments)]
fn draw_diff(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    files: &[StatusEntry],
    marked: &[bool],
    rows: &[DiffRow],
    selected: usize,
    content: &str,
    loading_new: bool,
    scroll: u16,
    h_scroll: u16,
    // Whether the changed-file list holds the keyboard. Always true on the
    // Changes tab; false in the three-panel Worktrees layout while the worktree
    // list above owns it. The diff pane is never focused either way.
    focused: bool,
    // `path_hit` is set to the screen rect of the diff panel's clickable path
    // title, so a click there can copy the path, or cleared when the cursor
    // isn't on a file.
    path_hit: &mut Option<Rect>,
    // Whether the diff pane draws its line-number gutter (`diff_line_numbers`).
    numbered: bool,
) -> Option<RowList> {
    *path_hit = None;
    if files.is_empty() {
        let para = Paragraph::new(Line::from("no uncommitted changes".dim()))
            .block(focus_panel(format!("changes · {name}"), focused));
        frame.render_widget(para, area);
        return None;
    }

    let [list_area, diff_area] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);

    // Left: the changed files as a folder tree, each row with a commit
    // checkbox. Folder rows show an aggregate mark ([x] all, [ ] none, [~]
    // some) over the files beneath them.
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            DiffRow::Folder {
                prefix,
                label,
                depth,
                collapsed,
            } => {
                let indent = "  ".repeat(*depth);
                let states: Vec<bool> = files
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.path.starts_with(prefix.as_str()))
                    .map(|(i, _)| marked.get(i).copied().unwrap_or(false))
                    .collect();
                let check = if states.iter().all(|s| *s) {
                    Span::styled("[x] ", Style::new().fg(theme::SUCCESS))
                } else if states.iter().any(|s| *s) {
                    Span::styled("[~] ", Style::new().fg(ACCENT))
                } else {
                    Span::styled("[ ] ", Style::new().dim())
                };
                let arrow = if *collapsed { "▸ " } else { "▾ " };
                ListItem::new(Line::from(vec![
                    check,
                    Span::raw(indent),
                    Span::styled(arrow, Style::new().fg(ACCENT)),
                    Span::styled(format!("{label}/"), Style::new().fg(ACCENT).bold()),
                ]))
            }
            DiffRow::File {
                index,
                label,
                depth,
            } => {
                let indent = "  ".repeat(*depth);
                let checked = marked.get(*index).copied().unwrap_or(false);
                let check = if checked {
                    Span::styled("[x] ", Style::new().fg(theme::SUCCESS))
                } else {
                    Span::styled("[ ] ", Style::new().dim())
                };
                let code = files.get(*index).map(|f| f.code.trim()).unwrap_or("");
                let style = files
                    .get(*index)
                    .map(|f| status_style(&f.code))
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    check,
                    Span::raw(indent),
                    Span::styled(format!("{code:<3}"), style),
                    Span::raw(label.clone()),
                ]))
            }
        })
        .collect();
    let block = focus_panel(format!("files · {name}"), focused);
    let inner = block.inner(list_area);
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol(Span::styled("▌", Style::new().fg(cursor_color(focused))));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let list_hit = RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    };

    // Right: the diff of the highlighted file, or a folder header when the
    // cursor rests on a folder row.
    let (title, lines): (String, Vec<Line>) = match rows.get(selected) {
        Some(DiffRow::Folder { prefix, .. }) => {
            let count = files
                .iter()
                .filter(|f| f.path.starts_with(prefix.as_str()))
                .count();
            (
                format!("folder · {prefix}"),
                vec![Line::from(
                    format!("{count} changed file(s) under {prefix}").dim(),
                )],
            )
        }
        _ => {
            let path = current_diff_path(rows, files, selected);
            // While a switch to a new file is still computing off-thread, show a
            // placeholder rather than the previous file's diff.
            let lines = if loading_new {
                vec![Line::from("loading diff…".dim())]
            } else if content.is_empty() {
                vec![Line::from("no textual diff (binary or empty)".dim())]
            } else {
                diff_lines_with_gutter(path, content, numbered)
            };
            let title = format!("diff · {path}");
            // The title doubles as a click-to-copy target for the path. `panel`
            // draws it one cell in from the border with a space either side, so
            // that is exactly the region to accept clicks in.
            if !path.is_empty() {
                *path_hit = Some(Rect {
                    x: diff_area.x + 1,
                    y: diff_area.y,
                    width: (title.chars().count() as u16 + 2)
                        .min(diff_area.width.saturating_sub(1)),
                    height: 1,
                });
            }
            (title, lines)
        }
    };
    let total = lines.len();
    let para = Paragraph::new(lines)
        .block(panel(title))
        .scroll((scroll, h_scroll));
    frame.render_widget(para, diff_area);
    let mut sb_state = ScrollbarState::new(total.saturating_sub(diff_area.height as usize))
        .position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::new().fg(BORDER))
            .thumb_style(Style::new().fg(ACCENT)),
        diff_area,
        &mut sb_state,
    );

    Some(list_hit)
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    // The error popup is modal and sits on top of everything else, so the
    // footer only shows how to read and dismiss it.
    if app.error.is_some() {
        frame.render_widget(
            Paragraph::new(hint_line_fitting(
                &[
                    hint("↑/↓", "scroll"),
                    hint("PgUp/PgDn", "page"),
                    hint("q/Esc", "dismiss"),
                ],
                Some(area.width),
            )),
            area,
        );
        return;
    }
    // The status message lives in the header now, so the key hints below stay
    // visible at all times.
    if app.show_help {
        frame.render_widget(
            Paragraph::new(hint_line_fitting(
                &[
                    hint("⇥", "tab"),
                    hint("↑/↓", "scroll"),
                    hint("Esc", "close"),
                ],
                Some(area.width),
            )),
            area,
        );
        return;
    }
    // A modal overlay owns the keys, so show its hints instead of the view's.
    if let Some(modal) = &app.modal {
        frame.render_widget(
            Paragraph::new(hint_line_fitting(
                modal_footer_hints(modal),
                Some(area.width),
            )),
            area,
        );
        return;
    }
    // An unfinished merge/rebase/cherry-pick, or a worktree left with unmerged
    // files, outranks the ordinary keymap: there is nothing to do with such a
    // worktree until it is resolved, so say how to get there rather than listing
    // keys that will only report "you have unmerged paths".
    //
    // Not when the resolver itself has the keyboard, though: in the three-panel
    // layout it lives inside the Worktrees tab, and pointing at a door the user
    // is already standing in would cost them the keys that actually work.
    let resolver_focused =
        app.resolver.is_some() && app.three_panel && app.worktrees_focus == WorktreesFocus::Files;
    if matches!(app.view, View::List)
        && app.tab == Tab::Worktrees
        && !resolver_focused
        && app
            .selected_worktree()
            .is_some_and(|w| w.conflicted > 0 || w.in_progress.is_some())
    {
        frame.render_widget(
            Paragraph::new(hint_line_fitting(
                &[
                    hint("Enter", "resolve conflicts"),
                    hint("x", "resolve conflicts"),
                    hint("↑/↓", "worktree"),
                    hint("?", "help"),
                ],
                Some(area.width),
            )),
            area,
        );
        return;
    }
    let hints: &[Binding] = match &app.view {
        View::List => match app.tab {
            // Three-panel layout: the hints follow the focused panel, since the
            // same keys act on a different list in each — and the bottom region
            // is the resolver while the worktree is conflicted.
            Tab::Worktrees
                if app.three_panel
                    && app.worktrees_focus == WorktreesFocus::Files
                    && app.resolver.is_some() =>
            {
                help::RESOLVER
            }
            Tab::Worktrees
                if app.three_panel
                    && app.worktrees_focus == WorktreesFocus::Files
                    && app.worktree_commits.is_some() =>
            {
                help::WORKTREE_COMMITS
            }
            Tab::Worktrees if app.three_panel && app.worktrees_focus == WorktreesFocus::Files => {
                help::WORKTREE_FILES
            }
            Tab::Worktrees if app.three_panel => help::WORKTREES_THREE_PANEL,
            Tab::Worktrees => help::WORKTREES,
            Tab::Branches => help::BRANCHES,
            Tab::Changes if app.resolver.is_some() => help::RESOLVER,
            Tab::Changes => help::DIFF,
            Tab::Stash => help::STASH_LIST,
            Tab::Settings if app.settings.is_typing() => {
                &[hint("Enter", "save value"), hint("Esc", "cancel edit")]
            }
            Tab::Settings if app.settings.open_list.is_some() => &[
                hint("↑/↓", "row"),
                hint("Enter", "edit"),
                hint("a", "add"),
                hint("d", "remove"),
                hint("g", "save globally"),
                hint("t", "run in terminal"),
                hint("[ done ]", "save"),
                hint("Esc", "discard"),
            ],
            Tab::Settings if app.settings.string_list.is_some() => &[
                hint("↑/↓", "row"),
                hint("Enter", "edit"),
                hint("a", "add"),
                hint("d", "remove"),
                hint("[ done ]", "save"),
                hint("Esc", "discard"),
            ],
            Tab::Settings => help::SETTINGS,
        },
        View::Log { .. } => &[
            hint("↑/↓", "commit"),
            hint("Enter", "browse files"),
            hint("u", "undo commit"),
            hint("g", "top"),
            hint("t", "tree/flat"),
            hint("q", "back"),
        ],
        View::CommitDiff { .. } => &[
            hint("↑/↓", "file"),
            hint("⇧↑/⇧↓", "scroll"),
            hint("⇧←/⇧→", "h-scroll"),
            hint("u", "undo commit"),
            hint("t", "tree/flat"),
            hint("q", "back"),
        ],
        View::StashDiff { .. } => &[
            hint("↑/↓", "file"),
            hint("⇧↑/⇧↓", "scroll"),
            hint("⇧←/⇧→", "h-scroll"),
            hint("t", "tree/flat"),
            hint("q", "back"),
        ],
        View::BranchCommits { .. } => help::BRANCH_COMMITS,
        View::CherryPick { mode: Some(_), .. } => &[
            hint("↑/↓", "mode"),
            hint("Enter", "confirm"),
            hint("Esc", "back"),
        ],
        View::CherryPick { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "choose mode"),
            hint("Esc", "cancel"),
        ],
        View::MergePick { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "merge"),
            hint("Esc", "cancel"),
        ],
        View::RebasePick { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "rebase"),
            hint("Esc", "cancel"),
        ],
        View::MoveChanges { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "move changes"),
            hint("Esc", "cancel"),
        ],
        View::OpenCommand { .. } => &[
            hint("↑/↓", "pick command"),
            hint("Enter", "run"),
            hint("Esc", "cancel"),
        ],
        View::UpstreamPick { .. } => &[
            hint("type", "filter"),
            hint("↑/↓", "pick"),
            hint("Enter", "set"),
            hint("Esc", "cancel"),
        ],
        View::StashTarget { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "apply"),
            hint("Esc", "cancel"),
        ],
        View::Commit { focus, .. } => match focus {
            CommitFocus::Files => help::COMMIT_FILES,
            CommitFocus::Message => &[
                hint("type", "commit message"),
                hint("Tab", "body"),
                hint("Enter", "commit"),
                hint("Esc", "cancel"),
            ],
            CommitFocus::Body => &[
                hint("type", "commit body"),
                hint("Enter", "new line"),
                hint("Tab", "pick files"),
                hint("^S", "commit"),
                hint("Esc", "cancel"),
            ],
        },
        View::Switch { .. } => &[
            hint("type", "filter"),
            hint("↑/↓", "select"),
            hint("Enter", "switch"),
            hint("Esc", "clear/close"),
        ],
        View::Busy { .. } => &[hint("", "working…")],
        View::Create {
            base_pick: Some(_), ..
        } => &[
            hint("↑/↓", "pick base branch"),
            hint("Enter", "use"),
            hint("Esc", "back"),
        ],
        View::Create {
            selected: 0,
            base_focus: true,
            ..
        } => &[
            hint("Enter/Space", "change base ⌄"),
            hint("Esc", "back to name"),
        ],
        View::Create { selected: 0, .. } => &[
            hint("type", "name / filter branches"),
            hint("⇥", "focus base ⌄"),
            hint("↓", "check out existing"),
            hint("Enter", "create"),
            hint("Esc", "cancel"),
        ],
        View::Create { .. } => &[
            hint("↑/↓", "pick branch"),
            hint("Enter", "check out"),
            hint("Esc", "cancel"),
        ],
        View::RunCommand { .. } => &[
            hint("type", "command to run in the worktree"),
            hint("Enter", "run"),
            hint("Esc", "cancel"),
        ],
        View::RenameWorktree { .. } => &[
            hint("type", "new worktree name"),
            hint("Enter", "rename"),
            hint("Esc", "cancel"),
        ],
        View::Creating { done: false, .. } => &[
            hint("type + Enter", "answer a prompt"),
            hint("Ctrl+C ×2", "kill setup"),
        ],
        View::Creating { .. } => &[hint("Enter", "done · back to worktrees")],
        View::Setup(wizard) => match &wizard.step {
            Step::Welcome { .. } => &[
                hint("↑/↓", "choose"),
                hint("Enter", "continue"),
                hint("Esc", "quit wtm"),
            ],
            Step::ClonePath { .. } => &[
                hint("type", "a path"),
                hint("Tab", "browse"),
                hint("Enter", "load"),
                hint("Esc", "back"),
            ],
            Step::CloneBrowse { .. } => &[
                hint("↑/↓", "select"),
                hint("Enter", "open/pick"),
                hint("Backspace", "up"),
                hint("Esc", "back"),
            ],
            Step::Location { .. } => &[
                hint("↑/↓", "choose"),
                hint("Enter", "continue"),
                hint("Esc", "back"),
            ],
            Step::LocationCustom { .. } | Step::CopyFiles { .. } => {
                &[hint("Enter", "continue"), hint("Esc", "back")]
            }
            Step::RunCommands { .. } => &[
                hint("Enter", "add command"),
                hint("blank Enter", "continue"),
                hint("Backspace", "undo last"),
                hint("Esc", "back"),
            ],
            Step::Review {
                editing: Some(_), ..
            } => &[hint("Enter", "save"), hint("Esc", "cancel edit")],
            Step::Review { .. } => &[
                hint("↑/↓", "select"),
                hint("Enter", "edit / write"),
                hint("Esc", "back"),
            ],
        },
    };
    frame.render_widget(
        Paragraph::new(hint_line_fitting(hints, Some(area.width))),
        area,
    );
}

/// Appends `value` to `spans` with a reverse-video block cursor at character
/// index `cursor`, so an inline editable field shows where edits will land.
fn push_cursor_spans(spans: &mut Vec<Span<'static>>, value: &str, cursor: usize, base: Style) {
    let byte = value
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(value.len());
    let (before, after) = value.split_at(byte);
    spans.push(Span::styled(before.to_string(), base));
    let mut rest = after.chars();
    match rest.next() {
        Some(under) => {
            spans.push(Span::styled(
                under.to_string(),
                base.bg(ACCENT).fg(Color::Black),
            ));
            spans.push(Span::styled(rest.collect::<String>(), base));
        }
        None => spans.push(Span::styled("▏".to_string(), base.fg(ACCENT))),
    }
}

/// Picks the visible character window for a single-line field that has `slot`
/// columns for text, keeping the cell at `cursor` inside it. `len` is the
/// character count; `cursor` may be `len` (the block cursor past the last
/// character), which is why the window may end one short of the text.
///
/// Returns the half-open character range to render.
fn cursor_window(len: usize, cursor: usize, slot: usize) -> (usize, usize) {
    let slot = slot.max(1);
    let cursor = cursor.min(len);
    // Scrolling right pins the cursor to the last column; the clamp stops the
    // window sliding past the end once the tail is on screen.
    let max_start = (len + 1).saturating_sub(slot);
    let start = cursor.saturating_sub(slot - 1).min(max_start);
    (start, (start + slot).min(len))
}

/// One editable line with a block cursor, limited to `width` columns: when the
/// text is longer, the visible window follows the cursor and `‹`/`›` mark the
/// clipped side, so typing past the field's width stays visible instead of
/// running off the edge with the cursor.
fn cursor_line_windowed(input: &str, cursor: usize, width: u16) -> Line<'static> {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    // One column beyond the text for the block cursor past the last character.
    let budget = width as usize;
    if len < budget {
        let mut spans = Vec::new();
        push_cursor_spans(&mut spans, input, cursor, Style::new());
        return Line::from(spans);
    }
    // A column at each end carries the elision marker (blank when that side
    // isn't clipped), so the text window doesn't jump as the cursor moves.
    let slot = budget.saturating_sub(2).max(1);
    let (start, end) = cursor_window(len, cursor, slot);
    let cursor = cursor.min(len);
    let marker = |clipped: bool, glyph: &str| {
        if clipped {
            Span::styled(glyph.to_string(), Style::new().fg(ACCENT).dim())
        } else {
            Span::raw(" ")
        }
    };
    let take = |range: std::ops::Range<usize>| chars[range].iter().collect::<String>();
    let mut spans = vec![
        marker(start > 0, "‹"),
        Span::raw(take(start..cursor.min(end))),
    ];
    if cursor < end {
        spans.push(Span::styled(
            chars[cursor].to_string(),
            Style::new().bg(ACCENT).fg(Color::Black),
        ));
        spans.push(Span::raw(take(cursor + 1..end)));
    } else {
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
    }
    spans.push(marker(end < len, "›"));
    Line::from(spans)
}

/// `cursor_line_windowed` behind the `❯ ` prompt prefix, for the dialogs whose
/// input is a full-width row.
fn prompt_line_windowed(input: &str, cursor: usize, width: u16) -> Line<'static> {
    let mut line = cursor_line_windowed(input, cursor, width.saturating_sub(2));
    line.spans
        .insert(0, Span::styled("❯ ", Style::new().fg(ACCENT).bold()));
    line
}

/// The typed input with a block cursor at the end, styled as a prompt line.
fn prompt_line(input: &str) -> Line<'static> {
    prompt_line_at(input, input.chars().count())
}

/// Like `prompt_line`, but draws the block cursor at character index `cursor`
/// so a field with a movable cursor shows where edits will land.
fn prompt_line_at(input: &str, cursor: usize) -> Line<'static> {
    let byte = input
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(input.len());
    let (before, after) = input.split_at(byte);
    let mut spans = vec![Span::styled("❯ ", Style::new().fg(ACCENT).bold())];
    spans.push(Span::raw(before.to_string()));
    let mut rest = after.chars();
    match rest.next() {
        // Draw the character under the cursor as a reverse-video block.
        Some(under) => {
            spans.push(Span::styled(
                under.to_string(),
                Style::new().bg(ACCENT).fg(Color::Black),
            ));
            spans.push(Span::raw(rest.collect::<String>()));
        }
        // Cursor at end: a thin bar after the text.
        None => spans.push(Span::styled("▏", Style::new().fg(ACCENT))),
    }
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
fn draw_create_dialog(
    frame: &mut Frame,
    area: Rect,
    name: &super::app::TextInput,
    branches: &[CheckoutCandidate],
    all_branches: &[String],
    base: &str,
    selected: usize,
    base_focus: bool,
    base_pick: Option<usize>,
    location: Option<&str>,
) {
    // The typed name doubles as a live filter over the checkout list, so only
    // matching candidates are shown (and navigable). `filtered` holds indices
    // into `branches`, matching the key handler's `filtered_candidates`.
    let filtered = filtered_candidates(branches, name.as_str());
    // Rows: the "new branch" action, then (only when there are branches to check
    // out) a section header, a blank spacer, and one row per matching branch.
    // The header and spacer come as a pair, hence `* 2`.
    let header_rows = usize::from(!filtered.is_empty());
    let list_rows = (1 + header_rows * 2 + filtered.len()).min(10) as u16;
    let popup = centered(area, 66, 7 + list_rows);
    frame.render_widget(Clear, popup);
    frame.render_widget(dialog_panel("new worktree"), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [name_area, base_area, list_area, base_hint_area, loc_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(list_rows + 1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(prompt_line_at(name.as_str(), name.cursor)),
        name_area,
    );

    // The base branch sits on its own row under the name, so a long branch name
    // never crowds the field being typed into. The `[ <branch> ⌄ ]` button is
    // filled and bold when Tab-focused, otherwise a bracketed accent chip; the
    // ⌄ signals it opens a dropdown of branches.
    let button_style = if base_focus {
        Style::new().fg(Color::Black).bg(ACCENT).bold()
    } else {
        Style::new().fg(ACCENT).bold()
    };
    // On a checkout row the typed text filters existing branches rather than
    // naming a new one, so the base does not apply: dim the whole row.
    let base_row_active = selected == 0;
    let base_hint = if base_focus {
        "Enter: pick · Esc: back"
    } else {
        "⇥ Tab"
    };
    // Budget for the branch name itself: the row's width less its own
    // decoration ("  ↳ off " + "[ " + " ⌄ ]") and the trailing hint.
    let base_budget = (base_area.width as usize)
        .saturating_sub(10 + 6 + base_hint.chars().count() + 2)
        .max(8);
    let base_display = truncate_middle(base, base_budget);
    let mut base_row = vec![
        Span::styled("  ↳ off ", Style::new().dim()),
        Span::styled("[", Style::new().dim()),
        Span::styled(format!(" {base_display} ⌄ "), button_style),
        Span::styled("]", Style::new().dim()),
    ];
    if base_row_active {
        base_row.push(Span::styled(format!("  {base_hint}"), Style::new().dim()));
    }
    let base_paragraph = if base_row_active {
        Paragraph::new(Line::from(base_row))
    } else {
        Paragraph::new(Line::from(base_row)).style(Style::new().dim())
    };
    frame.render_widget(base_paragraph, base_area);

    // Row 0: create a new branch off `base`; the section below checks out an
    // existing branch.
    let mut items: Vec<ListItem> = Vec::new();
    let typed = name.as_str().trim();
    let row0 = if typed.is_empty() {
        vec![
            Span::styled("+ ", Style::new().fg(Color::Green).bold()),
            Span::styled("type a name above to create a branch", Style::new().dim()),
        ]
    } else {
        vec![
            Span::styled("+ ", Style::new().fg(Color::Green).bold()),
            Span::raw(format!("new branch '{typed}'")),
        ]
    };
    items.push(ListItem::new(Line::from(row0)));
    if !filtered.is_empty() {
        let header = if name.as_str().trim().is_empty() {
            "  or check out an existing branch:".to_string()
        } else {
            format!("  or check out a match ({}):", filtered.len())
        };
        items.push(ListItem::new(Line::styled(header, Style::new().dim())));
        // Blank spacer so the header reads as a section break rather than as
        // another entry in the list. Non-selectable, like the header itself.
        items.push(ListItem::new(Line::from("")));
    }
    for &idx in &filtered {
        let candidate = &branches[idx];
        let mut spans = vec![
            Span::styled("⎇ ", Style::new().fg(ACCENT)),
            Span::raw(candidate.branch.clone()),
        ];
        // Flag remote-only branches (a teammate's work) so it is clear that
        // checking one out creates a local tracking branch.
        if let Some(remote) = &candidate.remote {
            spans.push(Span::styled(
                format!("  ({remote})"),
                Style::new().fg(ACCENT).dim(),
            ));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    // The section header and the spacer beneath it are both non-selectable, so
    // shift the highlight past the pair for any existing-branch selection.
    // While the base button is focused, drop the row highlight so only the
    // button reads as selected.
    let highlight_row = if base_focus {
        None
    } else if selected == 0 {
        Some(0)
    } else {
        Some(selected + 2)
    };
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default();
    state.select(highlight_row);
    frame.render_stateful_widget(list, list_area, &mut state);

    // The base-specific guidance now lives inline next to the chip, so this row
    // only carries what to press to finish.
    if selected == 0 && !base_focus {
        frame.render_widget(
            Paragraph::new(Line::styled("Enter: create", Style::new().dim())),
            base_hint_area,
        );
    }

    if let Some(location) = location {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("location: {location}"),
                Style::new().dim(),
            )),
            loc_area,
        );
    }

    // The base-branch picker floats over the dialog when active.
    if let Some(idx) = base_pick {
        draw_base_picker(frame, area, all_branches, idx);
    }
}

/// Floating list for choosing the base branch a new branch is created from.
fn draw_base_picker(frame: &mut Frame, area: Rect, all_branches: &[String], selected: usize) {
    let rows = all_branches.len().min(10) as u16;
    let popup = centered(area, 44, rows + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(dialog_panel("branch off of"), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
    let items: Vec<ListItem> = all_branches
        .iter()
        .map(|b| ListItem::new(Line::from(Span::raw(b.clone()))))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, inner, &mut state);
}

/// Rows the finished-state banner occupies: headline, path (or error), and
/// the "press Enter" line.
const CREATING_BANNER_ROWS: u16 = 3;

fn draw_creating(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    lines: &[String],
    outcome: Option<&CreateOutcome>,
    input: &str,
    kill_armed: bool,
) {
    let done = outcome.is_some();
    let input_rows = u16::from(!done);
    let banner_rows = if done { CREATING_BANNER_ROWS } else { 0 };
    let min_height = 4 + banner_rows;
    let height = (lines.len() as u16 + 2 + input_rows + banner_rows)
        .clamp(min_height, area.height.saturating_sub(2).max(min_height));
    let popup = centered(area, 76, height);
    frame.render_widget(Clear, popup);
    let title = match outcome {
        // The banner below says the same thing; the title mirrors it so the
        // state is legible even at a glance.
        Some(o) if o.ok => format!("creating {branch} · ready"),
        Some(_) => format!("creating {branch} · failed"),
        None => format!("creating {branch} · running…"),
    };
    frame.render_widget(dialog_panel(title), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
    let [log_area, banner_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(banner_rows)]).areas(inner);

    // Keep the tail visible when output exceeds the popup. Long lines wrap, so
    // budget by rendered rows rather than by entries: counting entries lets a
    // few wrapped lines overflow the area and pin the log to its *head*.
    let capacity = log_area.height.saturating_sub(input_rows) as usize;
    let width = log_area.width.max(1) as usize;
    let mut used = 0;
    let mut skip = lines.len();
    for (i, line) in lines.iter().enumerate().rev() {
        let rows = line.chars().count().div_ceil(width).max(1);
        if used + rows > capacity {
            break;
        }
        used += rows;
        skip = i;
    }
    let mut text: Vec<Line> = lines[skip..].iter().map(|l| output_line(l)).collect();
    if !done {
        if kill_armed {
            text.push(Line::styled(
                "press Ctrl+C again to kill the setup",
                Style::new().fg(theme::DANGER).bold(),
            ));
        } else {
            text.push(prompt_line(input));
        }
    }
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), log_area);

    // A filled banner pinned to the bottom of the popup: unlike a line pushed
    // into the log above, a long `npm install` can't scroll it away.
    if let Some(outcome) = outcome {
        let (bg, headline, detail) = if outcome.ok {
            (
                theme::SUCCESS,
                "  ✓  READY — worktree set up and ready to use",
                outcome.path.clone(),
            )
        } else if outcome.path.is_empty() {
            (
                theme::DANGER,
                "  ✗  FAILED — worktree creation failed",
                outcome.detail.clone().unwrap_or_default(),
            )
        } else {
            (
                theme::DANGER,
                "  ✗  FAILED — worktree created, but setup had errors",
                outcome.path.clone(),
            )
        };
        // The banner is fixed-height, so elide the middle of a long path
        // rather than letting the right edge cut off the worktree name.
        let detail = truncate_middle(&detail, banner_area.width.saturating_sub(2) as usize);
        let banner = Paragraph::new(vec![
            Line::styled(headline, Style::new().bold()),
            Line::raw(format!("  {detail}")),
            Line::styled("  press Enter to continue", Style::new().bold()),
        ])
        .style(Style::new().bg(bg).fg(Color::Black));
        frame.render_widget(banner, banner_area);
    }
}

/// Prompt for a one-off command to run in a worktree's directory.
fn draw_run_command(frame: &mut Frame, area: Rect, name: &str, input: &super::app::TextInput) {
    let popup = centered(area, 64, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(dialog_panel(format!("run in '{name}'")), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [prompt_area, hint_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(prompt_line_at(input.as_str(), input.cursor)),
        prompt_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "e.g. open {path}  ·  set open_command in Settings to skip this prompt",
            Style::new().dim(),
        )),
        hint_area,
    );
}

/// The worktree rename prompt: a small centered dialog with the new name,
/// prefilled with the current one.
fn draw_rename_worktree(frame: &mut Frame, area: Rect, name: &str, input: &super::app::TextInput) {
    let popup = centered(area, 64, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(dialog_panel(format!("rename '{name}'")), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [prompt_area, hint_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(prompt_line_at(input.as_str(), input.cursor)),
        prompt_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            "renames the branch and moves the directory · Esc cancels",
            Style::new().dim(),
        )),
        hint_area,
    );
}

/// Styles one line of setup output: step results and errors stand out,
/// echoed user input shows its prompt, plain command output stays dim.
fn output_line(line: &str) -> Line<'_> {
    let style = if line.starts_with("[ok]") || line.starts_with('✓') {
        Style::new().fg(theme::SUCCESS).bold()
    } else if line.starts_with("[FAILED]") || line.starts_with("error") || line.starts_with('✗') {
        Style::new().fg(theme::DANGER).bold()
    } else if line.starts_with('═') || line.starts_with("──") {
        Style::new().fg(ACCENT).bold()
    } else if line.starts_with("❯ ") {
        Style::new().fg(ACCENT)
    } else if line.starts_with("creating ")
        || line.starts_with("running:")
        || line.starts_with("worktree ")
        || line.starts_with("press ")
    {
        Style::new()
    } else {
        Style::new().dim()
    };
    Line::from(Span::styled(line, style))
}

/// The help panel: a tabbed, scrollable overlay. Content comes from the
/// `help` registry, the same data the footer hints are built from.
/// Width of the help panel's key column (`"  {key:<12}"`), used so wrapped
/// description lines hang under the description rather than the left edge.
const HELP_KEY_COL: usize = 14;

/// Push hard-split chunks of an overlong `word` into `lines`, leaving any
/// final partial chunk in `current` so the next word can still join it.
fn push_overlong(word: &str, width: usize, lines: &mut Vec<String>, current: &mut String) {
    let mut rest = word;
    while !rest.is_empty() {
        let mut chars = rest.chars();
        let take: String = chars.by_ref().take(width).collect();
        rest = chars.as_str();
        if rest.is_empty() {
            *current = take;
        } else {
            lines.push(take);
        }
    }
}

/// Word-wrap `text` into lines of at most `width` characters.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            if word_len > width {
                push_overlong(word, width, &mut lines, &mut current);
            } else {
                current.push_str(word);
            }
            continue;
        }
        if current.chars().count() + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            continue;
        }
        lines.push(std::mem::take(&mut current));
        if word_len > width {
            push_overlong(word, width, &mut lines, &mut current);
        } else {
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// One help binding as one or more lines, with continuation lines indented
/// under the description (after the key column).
fn help_binding_lines(key: &str, label: &str, width: usize) -> Vec<Line<'static>> {
    let key_span = Span::styled(format!("  {key:<12}"), Style::new().fg(ACCENT).bold());
    let desc_width = width.saturating_sub(HELP_KEY_COL).max(1);
    let chunks = wrap_words(label, desc_width);
    let indent = " ".repeat(HELP_KEY_COL);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            if i == 0 {
                Line::from(vec![key_span.clone(), Span::raw(chunk)])
            } else {
                Line::from(vec![Span::raw(indent.clone()), Span::raw(chunk)])
            }
        })
        .collect()
}

fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let heading =
        |t: &str| -> Line<'static> { Line::from(Span::styled(t.to_string(), Style::new().bold())) };

    // Wrap against the panel's inner width before sizing height, so long
    // descriptions contribute the right number of scroll rows.
    const HELP_WIDTH: u16 = 78;
    // Borders (2) + horizontal padding (2) match `panel`.
    let body_width = HELP_WIDTH.min(area.width).saturating_sub(4) as usize;

    let mut text: Vec<Line> = Vec::new();
    for section in help::sections(app.help_tab) {
        if !text.is_empty() {
            text.push(Line::from(""));
        }
        text.push(heading(section.heading));
        for b in section.bindings {
            text.extend(help_binding_lines(b.key, b.long, body_width));
        }
        for note in section.notes {
            text.push(Line::from(format!("  {note}").dim()));
        }
    }

    // Size to the content so short tabs get a small panel, but never grow past
    // the terminal: the old fixed 58-row popup silently lost its tail on short
    // screens. 4 = the block's two borders plus the tab bar and its spacer.
    const CHROME: u16 = 4;
    let content_height = text.len() as u16;
    let popup = modal_rect(area, content_height, HELP_WIDTH, CHROME);
    frame.render_widget(Clear, popup);
    let block = dialog_panel("help");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [bar, _gap, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);
    draw_help_tabs(frame, bar, app.help_tab);

    // Clamp here rather than in `App`, as the diff and log views do: the
    // viewport height is only known at render time.
    let max_scroll = content_height.saturating_sub(body.height);
    let scroll = app.help_scroll.min(max_scroll);
    // Descriptions are pre-wrapped with a hanging indent; leave Wrap off so
    // ratatui does not reflow continuation lines back to column 0.
    frame.render_widget(Paragraph::new(text).scroll((scroll, 0)), body);
    if max_scroll > 0 {
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            body,
            &mut ScrollbarState::new(max_scroll as usize).position(scroll as usize),
        );
    }
}

/// The help panel's tab bar, styled to match the main window's (`draw_tab_bar`).
fn draw_help_tabs(frame: &mut Frame, area: Rect, active: HelpTab) {
    let mut spans = Vec::new();
    for tab in HelpTab::ALL {
        if tab == active {
            spans.push(Span::styled(
                format!(" {} ", tab.title()),
                Style::new().fg(Color::Black).bg(ACCENT).bold(),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {} ", tab.title()),
                Style::new().fg(BORDER),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A centered, red-bordered popup for `app.error`: unlike the one-line status
/// message in the header, this shows a full multi-line git error, scrolls when
/// the message is taller than the popup, and is dismissed only by q/Esc/Enter
/// (see `App::error`, `App::on_error_key`).
///
/// Returns the largest useful scroll offset, which the caller stores on the app
/// so the scroll keys can clamp against it.
fn draw_error_popup(frame: &mut Frame, area: Rect, msg: &str, scroll: u16) -> u16 {
    let width = 70.min(area.width);
    // Inner content width, accounting for the block's border and padding, used
    // to estimate how many visual lines the wrapped message will take.
    let inner_width = width.saturating_sub(4).max(1) as usize;
    let wrapped_lines: usize = msg
        .lines()
        .map(|line| line.chars().count().div_ceil(inner_width).max(1))
        .sum();
    // +2 for the border, +2 for the blank line and dismiss hint below the
    // message. `modal_rect` clamps the height, so a long error becomes a
    // scrollable popup rather than one that overflows the frame.
    let popup = modal_rect(area, wrapped_lines as u16, width, 4);
    // Rows actually available to the message inside the popup, i.e. its height
    // less the border and the blank + hint that always stay pinned at the end.
    let visible = popup.height.saturating_sub(4).max(1);
    let max_scroll = (wrapped_lines as u16).saturating_sub(visible);
    let scroll = scroll.min(max_scroll);
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = msg.lines().map(Line::from).collect();
    let title_suffix = if max_scroll > 0 {
        // Which slice of the message is on screen, so a truncated-looking
        // popup reads as "there is more" rather than as the whole error.
        let last = (scroll + visible).min(wrapped_lines as u16);
        format!(" {}-{} of {} ", scroll + 1, last, wrapped_lines)
    } else {
        String::new()
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(theme::DANGER))
        .style(Style::new().bg(DIALOG_BG))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "error",
                Style::new().fg(theme::DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(title_suffix, Style::new().dim()),
            Span::raw(" "),
        ]));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // The hint stays pinned to the bottom of the popup: only the message
    // scrolls, so "how do I close this" never scrolls out of view.
    let [msg_area, _, hint_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        msg_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            if max_scroll > 0 {
                "↑/↓ or wheel scroll · q / Esc to dismiss"
            } else {
                "q / Esc to dismiss"
            },
            Style::new().dim(),
        )),
        hint_area,
    );
    max_scroll
}

/// Renders the current screen of the first-run setup wizard. Every screen is a
/// centered panel titled with its question and a `step N of M` label, and each
/// question carries a short blurb saying why it is being asked.
fn draw_setup(frame: &mut Frame, area: Rect, wizard: &SetupWizard) -> Option<RowList> {
    let progress = wizard.progress();
    let progress = progress.as_str();
    match &wizard.step {
        Step::Welcome { selected } => draw_welcome(frame, area, wizard, *selected, progress),
        Step::ClonePath { input } => {
            draw_clone_path(frame, area, input, progress);
            None
        }
        Step::CloneBrowse { browser, .. } => draw_browser(frame, area, browser, progress),
        Step::Location { selected } => draw_location(frame, area, wizard, *selected, progress),
        Step::LocationCustom { input } => {
            draw_wizard_input(
                frame,
                area,
                "Type a worktree location",
                progress,
                &[
                    "An absolute path, a ~/… path, or a path relative to the repo.",
                    "{repo} is replaced with this repo's folder name.",
                ],
                input,
                "e.g. ~/code/worktrees/{repo}",
            );
            None
        }
        Step::CopyFiles { input } => {
            draw_wizard_input(
                frame,
                area,
                "Which files should be copied in?",
                progress,
                &[
                    "A new worktree is a clean checkout, so anything git ignores",
                    "(your .env, local credentials) won't be there. wtm copies the",
                    "files listed here from this repo into every worktree it makes.",
                ],
                input,
                "comma separated · leave blank to copy nothing",
            );
            None
        }
        Step::RunCommands { commands, input } => {
            draw_run_commands(frame, area, commands, input, progress);
            None
        }
        Step::Review { selected, editing } => {
            draw_review(frame, area, wizard, *selected, editing.as_ref(), progress)
        }
    }
}

/// Width every wizard panel shares, so the screens don't jump around as the
/// user moves between them. Clamped to the terminal by `centered`.
const WIZARD_WIDTH: u16 = 84;

/// Joins a step's own title with the shared progress label for its panel.
fn wizard_title(title: &str, progress: &str) -> String {
    format!("{title}  ·  {progress}")
}

/// Dim explanatory lines shown above a wizard question.
fn blurb_lines(blurb: &[&str]) -> Vec<Line<'static>> {
    blurb
        .iter()
        .map(|line| Line::from(line.to_string().dim()))
        .collect()
}

/// Lays out a wizard screen: a panel whose inner area is the blurb, a blank
/// line, then `body_height` rows for the question itself. Returns the body rect
/// so list-based steps can render into it and report click geometry.
fn wizard_screen(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    progress: &str,
    blurb: &[&str],
    body_height: u16,
) -> Rect {
    let blurb_height = blurb.len() as u16;
    let gap = if blurb.is_empty() { 0 } else { 1 };
    let popup = centered(area, WIZARD_WIDTH, blurb_height + gap + body_height + 2);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(wizard_title(title, progress));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if !blurb.is_empty() {
        let head = Rect {
            height: blurb_height.min(inner.height),
            ..inner
        };
        frame.render_widget(Paragraph::new(blurb_lines(blurb)), head);
    }
    let used = (blurb_height + gap).min(inner.height);
    Rect {
        y: inner.y + used,
        height: inner.height - used,
        ..inner
    }
}

/// Welcome screen: what wtm does, what setup writes, and the two routes.
fn draw_welcome(
    frame: &mut Frame,
    area: Rect,
    wizard: &SetupWizard,
    selected: usize,
    progress: &str,
) -> Option<RowList> {
    let repo = wizard
        .repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| wizard.repo_root.display().to_string());
    let heading = format!("{repo} isn't set up for wtm yet.");
    let blurb = [
        heading.as_str(),
        "",
        "wtm gives each branch its own folder (a git worktree), so you and any",
        "agents can work on several branches at once without stashing or",
        "switching. Creating one is a single keypress once this is set up.",
        "",
        "Setup writes one file, .wtm.toml, in the repo root. It records where",
        "those folders go and how to make a new one usable: which ignored files",
        "to copy in and which commands to run. Nothing else is touched, and you",
        "can change any of it later on the Settings tab.",
    ];
    let body = wizard_screen(
        frame,
        area,
        "Welcome to wtm",
        progress,
        &blurb,
        WELCOME_OPTIONS.len() as u16,
    );
    let items: Vec<ListItem> = WELCOME_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, (label, detail))| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{}. ", i + 1), Style::new().dim()),
                Span::styled(label.to_string(), Style::new().bold()),
                Span::styled(format!("  ·  {detail}"), Style::new().dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, body, &mut state);
    Some(RowList {
        inner: body,
        header: 0,
        offset: state.offset(),
        len: WELCOME_OPTIONS.len(),
    })
}

fn draw_clone_path(frame: &mut Frame, area: Rect, input: &super::app::TextInput, progress: &str) {
    let body = wizard_screen(
        frame,
        area,
        "Copy settings from where?",
        progress,
        &[
            "Point at a repo that already uses wtm (or straight at a .wtm.toml)",
            "and its answers become this repo's starting point. You get to review",
            "and edit them before anything is written.",
        ],
        2,
    );
    let lines = vec![
        prompt_line_at(input.as_str(), input.cursor),
        Line::from("path to a repo or a .wtm.toml file · Tab opens a file browser".dim()),
    ];
    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_browser(
    frame: &mut Frame,
    area: Rect,
    browser: &super::setup::FileBrowser,
    progress: &str,
) -> Option<RowList> {
    let height = (browser.entries.len() as u16 + 2).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = centered(area, WIZARD_WIDTH, height);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = if browser.entries.is_empty() {
        vec![ListItem::new(Line::from(
            "(no folders or .toml files here)".dim(),
        ))]
    } else {
        browser
            .entries
            .iter()
            .map(|entry| {
                let line = if entry.is_dir {
                    Line::from(Span::styled(
                        format!("{}/", entry.name),
                        Style::new().bold().fg(ACCENT),
                    ))
                } else {
                    Line::from(entry.name.clone())
                };
                ListItem::new(line)
            })
            .collect()
    };
    let block = dialog_panel(wizard_title(&browser.dir.display().to_string(), progress))
        .title_bottom(Line::from(" pick a repo folder or a .wtm.toml ".dim()).right_aligned());
    let inner = block.inner(popup);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(browser.selected));
    frame.render_stateful_widget(list, popup, &mut state);
    (!browser.entries.is_empty()).then_some(RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: browser.entries.len(),
    })
}

fn draw_location(
    frame: &mut Frame,
    area: Rect,
    wizard: &SetupWizard,
    selected: usize,
    progress: &str,
) -> Option<RowList> {
    let body = wizard_screen(
        frame,
        area,
        "Where should worktree folders go?",
        progress,
        &[
            "Each worktree is a real folder on disk holding one branch. Pick where",
            "wtm should create them; the resolved path is shown for each choice.",
        ],
        LOCATION_PRESETS.len() as u16 + 1,
    );
    let mut items: Vec<ListItem> = LOCATION_PRESETS
        .iter()
        .map(|(name, label)| {
            let preview = location_preview(name, &wizard.repo_root);
            ListItem::new(Line::from(vec![
                Span::styled(label.to_string(), Style::new().bold()),
                Span::styled(format!("  ·  {preview}"), Style::new().dim()),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(vec![
        Span::styled("somewhere else", Style::new().bold()),
        Span::styled("  ·  type your own path", Style::new().dim()),
    ])));
    let len = items.len();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, body, &mut state);
    Some(RowList {
        inner: body,
        header: 0,
        offset: state.offset(),
        len,
    })
}

/// A wizard screen whose body is one text input with a hint underneath.
fn draw_wizard_input(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    progress: &str,
    blurb: &[&str],
    input: &super::app::TextInput,
    hint: &str,
) {
    let body = wizard_screen(frame, area, title, progress, blurb, 2);
    let lines = vec![
        prompt_line_at(input.as_str(), input.cursor),
        Line::from(hint.to_string().dim()),
    ];
    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_run_commands(
    frame: &mut Frame,
    area: Rect,
    commands: &[String],
    input: &super::app::TextInput,
    progress: &str,
) {
    // One row per command already added, the input line, and the hint under it.
    let body_height = commands.len() as u16 + 2;
    let body = wizard_screen(
        frame,
        area,
        "What should run in a new worktree?",
        progress,
        &[
            "Commands wtm runs inside each new worktree once it exists, in order,",
            "so the branch is ready to work on: installing dependencies, building,",
            "generating clients. Add them one per line.",
        ],
        body_height,
    );
    let mut lines: Vec<Line> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            Line::from(vec![
                Span::styled(format!("{}. ", i + 1), Style::new().dim()),
                Span::styled(cmd.clone(), Style::new().fg(theme::SUCCESS)),
            ])
        })
        .collect();
    lines.push(prompt_line_at(input.as_str(), input.cursor));
    // The footer carries Backspace-to-undo, so this only needs to explain the
    // one non-obvious part: a blank line is how you finish.
    lines.push(Line::from(
        "one per line · Enter on a blank line moves on".dim(),
    ));
    frame.render_widget(Paragraph::new(lines), body);
}

/// Review screen: the three answers under plain-English labels, then the row
/// that writes the file. Any answer can still be edited in place.
fn draw_review(
    frame: &mut Frame,
    area: Rect,
    wizard: &SetupWizard,
    selected: usize,
    editing: Option<&super::app::TextInput>,
    progress: &str,
) -> Option<RowList> {
    let none = "(none)".to_string();
    let values = [
        // The preset's label reads better here than the raw `sibling`/`home`
        // keyword, but an edit still gets the raw value to work on.
        location_label(&wizard.draft.worktree_dir).to_string(),
        if wizard.draft.copy.is_empty() {
            none.clone()
        } else {
            wizard.draft.copy.join(", ")
        },
        if wizard.draft.run.is_empty() {
            none
        } else {
            wizard.draft.run.join(", ")
        },
    ];
    // A cloned absolute path usually points at the other repo's location, so
    // it's worth a second look before writing.
    let warn = wizard.cloned
        && (wizard.draft.worktree_dir.starts_with('/')
            || wizard.draft.worktree_dir.starts_with('~'));
    // Three field rows, a blank separator, the write row, then the resolved
    // location and (sometimes) the warning: the line layout `on_click` decodes.
    let body_height = 3 + 1 + 1 + 1 + u16::from(warn);
    let body = wizard_screen(
        frame,
        area,
        "Ready to write",
        progress,
        &[
            "This is what goes into .wtm.toml. Enter on a row edits it; the last",
            "row writes the file and opens the repo.",
        ],
        body_height,
    );
    let labels = ["Worktree folders", "Files to copy   ", "Commands to run "];
    let mut lines: Vec<Line> = Vec::new();
    for (row, label) in labels.iter().enumerate() {
        let highlight = if row == selected {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        };
        let mut spans = vec![Span::styled(format!(" {label}  "), highlight.bold())];
        match (row == selected, editing) {
            (true, Some(input)) => {
                push_cursor_spans(&mut spans, input.as_str(), input.cursor, highlight)
            }
            _ => spans.push(Span::styled(values[row].clone(), highlight)),
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    let write_row = REVIEW_ROWS - 1;
    let write_style = if selected == write_row {
        Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
    } else {
        Style::new().bold()
    };
    lines.push(Line::from(Span::styled(
        " [ Write .wtm.toml and start ] ",
        write_style,
    )));
    let row_lines = labels.len() as u16 + 2;
    lines.push(Line::from(
        format!(
            "new worktrees will land in {}",
            location_preview(&wizard.draft.worktree_dir, &wizard.repo_root)
        )
        .dim(),
    ));
    if warn {
        lines.push(Line::from(Span::styled(
            "this path came from the other repo; check it suits this one",
            Style::new().fg(theme::WARNING),
        )));
    }
    frame.render_widget(Paragraph::new(lines), body);
    Some(RowList {
        inner: body,
        header: 0,
        offset: 0,
        len: row_lines as usize,
    })
}

/// The Settings tab: spaced setting blocks with descriptions, grouped into
/// repo vs all-repos sections, plus a live theme sample, worktree-location
/// preview, version line, and check-for-updates action. Changes write to disk
/// as soon as they are made. The form keeps a fixed width and scrolls so the
/// selected row stays visible on short terminals.
fn draw_settings_tab(
    frame: &mut Frame,
    area: Rect,
    editor: &ConfigEditor,
    update_available: Option<&Release>,
) -> Option<RowList> {
    let labels = [
        "worktree_dir",
        "open_command",
        "setup.copy",
        "setup.run",
        "auto_update_check",
        "diff_theme",
        "worktrees_layout",
        "branches_refresh_mins",
        "diff_line_numbers",
        "conflict_editor",
    ];
    // Keep each description to one line at the form width (78) so wrapping
    // cannot desync [`line_of_row`] from what is drawn.
    let descriptions = [
        "Where new worktrees go: sibling, inside, home, or a path ({repo} = name).",
        "Commands the o key runs. Enter edits the list ({path} {name} {branch}).",
        "Files copied into each new worktree. Enter edits the list (e.g. .env).",
        "Commands run in each new worktree after create. Enter edits the list.",
        "Check GitHub for a newer wtm when the TUI starts. Enter cycles.",
        "Syntax colours in the diff pane. Enter cycles themes.",
        "Worktrees tab layout. Three panels add files + diff. Enter cycles.",
        "Minutes the Branches tab keeps its list before refreshing.",
        "Show a line-number gutter beside the diff. Enter cycles.",
        "Editor the resolver's e key uses ({path}). Enter edits, Space flips mode.",
    ];
    let mut lines: Vec<Line> = Vec::new();

    let push_section = |lines: &mut Vec<Line>, title: &str| {
        lines.push(Line::from(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        )));
        lines.push(Line::from(""));
    };

    push_section(&mut lines, "── This repo (.wtm.toml) ──");

    for row in 0..FIELD_ROWS {
        if row == UPDATE_ROW {
            push_section(&mut lines, "── All repos ──");
        }

        let selected = row == editor.selected;
        let highlight = if selected {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        };
        // Pad labels to a shared column so values line up across settings.
        let mut spans = vec![Span::styled(
            format!(" {:<22} ", labels[row]),
            highlight.fg(ACCENT).bold(),
        )];
        match (selected, &editor.editing) {
            // The row being edited shows the live buffer with a movable cursor.
            (true, Some(input)) => {
                push_cursor_spans(&mut spans, input.as_str(), input.cursor, highlight)
            }
            // The toggle has no free text, so it renders its state rather than
            // an editable value, spelling out what the default resolves to.
            _ if row == UPDATE_ROW => spans.push(Span::styled(
                match editor.fields.auto_update_check.as_str() {
                    "true" => "on".to_string(),
                    "false" => "off".to_string(),
                    _ => format!(
                        "(default: {})",
                        if DEFAULT_AUTO_UPDATE_CHECK {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                },
                highlight,
            )),
            _ if row == THEME_ROW => spans.push(Span::styled(
                if editor.fields.diff_theme.is_empty() {
                    format!("(default: {})", highlight::theme_label(DEFAULT_DIFF_THEME))
                } else {
                    highlight::theme_label(&editor.fields.diff_theme).to_string()
                },
                highlight,
            )),
            _ if row == LAYOUT_ROW => spans.push(Span::styled(
                if editor.fields.worktrees_layout.is_empty() {
                    format!(
                        "(default: {})",
                        worktrees_layout_label(WorktreesLayout::default().as_str())
                    )
                } else {
                    worktrees_layout_label(&editor.fields.worktrees_layout).to_string()
                },
                highlight,
            )),
            _ if row == DIFF_LINE_NUMBERS_ROW => spans.push(Span::styled(
                match editor.fields.diff_line_numbers.as_str() {
                    "true" => "on".to_string(),
                    "false" => "off".to_string(),
                    _ => format!(
                        "(default: {})",
                        if DEFAULT_DIFF_LINE_NUMBERS {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                },
                highlight,
            )),
            _ if row == BRANCHES_REFRESH_ROW => spans.push(Span::styled(
                if editor.fields.branches_refresh_mins.is_empty() {
                    format!("(default: {DEFAULT_BRANCHES_REFRESH_MINS})")
                } else {
                    editor.fields.branches_refresh_mins.clone()
                },
                highlight,
            )),
            // List rows show a one-line summary of their entries; Enter
            // opens the list editor drawn over the form below.
            _ if row == OPEN_COMMAND_ROW && !editor.fields.open_command.is_empty() => {
                spans.push(Span::styled(editor.open_command_summary(), highlight))
            }
            _ if row == COPY_ROW && !editor.fields.copy.is_empty() => {
                spans.push(Span::styled(editor.copy_summary(), highlight))
            }
            _ if row == RUN_ROW && !editor.fields.run.is_empty() => {
                spans.push(Span::styled(editor.run_summary(), highlight))
            }
            _ if row == CONFLICT_EDITOR_ROW => {
                spans.push(if editor.fields.conflict_editor.is_some() {
                    Span::styled(editor.conflict_editor_summary(), highlight)
                } else {
                    Span::styled("(default: built-in editor)".to_string(), highlight.dim())
                })
            }
            _ if editor.field(row).is_empty() => {
                spans.push(Span::styled("(default)".to_string(), highlight.dim()))
            }
            _ => spans.push(Span::styled(editor.field(row).to_string(), highlight)),
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(Span::styled(
            format!("   {}", descriptions[row]),
            Style::new().dim(),
        )));

        // Live colour sample sits under the theme description so cycling
        // `diff_theme` shows the palette next to that setting.
        if row == THEME_ROW {
            let theme_id = if editor.fields.diff_theme.is_empty() {
                DEFAULT_DIFF_THEME
            } else {
                editor.fields.diff_theme.as_str()
            };
            lines.push(Line::from(Span::styled(
                format!(
                    " → diff colours look like ({})",
                    highlight::theme_label(theme_id)
                ),
                Style::new().fg(Color::Green),
            )));
            let samples = highlight::theme_preview_lines(theme_id);
            debug_assert_eq!(samples.len(), THEME_PREVIEW_SAMPLE_LINES);
            for sample in samples {
                let mut spans = vec![Span::raw("   ")];
                spans.extend(sample.spans);
                lines.push(Line::from(spans));
            }
        }

        // Blank separator so the next setting (or section) reads as its own block.
        lines.push(Line::from(""));
    }

    debug_assert_eq!(
        lines.len(),
        preview_line(),
        "form line map drifted from draw"
    );

    // Live preview of where worktrees will actually be created.
    let raw_dir = if editor.fields.worktree_dir.trim().is_empty() {
        DEFAULT_LOCATION
    } else {
        editor.fields.worktree_dir.trim()
    };
    let resolved = crate::config::resolve_worktree_dir(raw_dir, &editor.repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(needs HOME set)".to_string());
    lines.push(Line::from(Span::styled(
        format!(" → new worktrees go in {resolved}"),
        Style::new().fg(Color::Green),
    )));

    // Running version, plus whatever the last update check turned up.
    let mut version = vec![Span::styled(
        format!(" wtm {CURRENT_VERSION}"),
        Style::new().dim(),
    )];
    match update_available {
        Some(release) => version.push(Span::styled(
            format!("  ·  {} available", release.version),
            Style::new().fg(Color::Yellow).bold(),
        )),
        None => version.push(Span::styled("  ·  up to date", Style::new().dim())),
    }
    lines.push(Line::from(version));

    let action_style = |row: usize| {
        if editor.selected == row {
            Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
        } else {
            Style::new().bold()
        }
    };
    lines.push(Line::from(Span::styled(
        " [ check for updates now ] ",
        action_style(CHECK_ROW),
    )));

    debug_assert_eq!(lines.len(), form_lines());

    let block = panel("settings");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The form is a fixed-width column centered in the panel, with one blank
    // line above it so it doesn't sit flush against the border.
    let [_, form, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(78),
        Constraint::Min(0),
    ])
    .areas(inner);
    let [_, form] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(form);

    // Keep the selected value line (or check-now action) in view when the form
    // is taller than the panel.
    let focus_line = if editor.selected == CHECK_ROW {
        check_line()
    } else {
        line_of_row(editor.selected)
    };
    let visible = form.height as usize;
    let total = form_lines();
    let mut scroll = 0usize;
    if visible > 0 && focus_line >= visible {
        scroll = (focus_line + 1).saturating_sub(visible);
        let max_scroll = total.saturating_sub(visible);
        scroll = scroll.min(max_scroll);
    }

    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), form);
    // The list editor is modal over the form: it takes every key, so the form
    // reports no clickable rows while it is up.
    if let Some(list) = &editor.open_list {
        draw_open_command_list(frame, area, list);
        return None;
    }
    if let Some(list) = &editor.string_list {
        draw_string_list(frame, area, list);
        return None;
    }
    // The line layout is shared with `config_editor::row_at_line`, which
    // `on_click` uses to turn a clicked line back into a row. `offset` is the
    // scroll so a click on the visible viewport maps to the right form line.
    Some(RowList {
        inner: form,
        header: 0,
        offset: scroll,
        len: total,
    })
}

/// The `open_command` list editor, floating over the Settings form: one row
/// per configured command, then an add row and a done row. The row being
/// typed shows the live buffer with a movable cursor.
fn draw_open_command_list(frame: &mut Frame, area: Rect, list: &OpenCommandEditor) {
    let rows = list.rows().clamp(3, 14) as u16;
    let popup = centered(area, 84, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel("open commands");
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(
            "the open key (o) runs these · {path} {name} {branch} {status} expand on run".dim(),
        )),
        head_area,
    );

    let row_style = |row: usize| {
        if list.selected == row {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    for (index, command) in list.commands.iter().enumerate() {
        let style = row_style(index);
        // Both toggles sit in fixed-width columns *before* the command, so a
        // long template truncates on the right instead of pushing them off
        // screen (and the columns stay aligned down the list).
        let (scope, scope_style) = if command.global {
            ("global", style.fg(theme::INFO))
        } else {
            ("repo  ", style.dim())
        };
        let (mode, mode_style) = if command.mode == CommandMode::Terminal {
            ("terminal  ", style.fg(theme::WARNING))
        } else {
            ("background", style.dim())
        };
        let mut spans = vec![
            Span::styled(" ● ", style.fg(Color::Green)),
            Span::styled(scope, scope_style),
            Span::styled(" · ", style.dim()),
            Span::styled(mode, mode_style),
            Span::styled("  ", style),
        ];
        match &list.input {
            Some(input) if list.editing_index == Some(index) => {
                push_cursor_spans(&mut spans, input.as_str(), input.cursor, style)
            }
            _ => spans.push(Span::styled(command.command.clone(), style)),
        }
        lines.push(Line::from(spans));
    }
    let add_style = row_style(list.add_row());
    match &list.input {
        // A new entry is typed on the add row itself, so it reads as the
        // command being appended rather than as a detached prompt.
        Some(input) if list.editing_index.is_none() => {
            let mut spans = vec![Span::styled(" + ", add_style.fg(ACCENT))];
            push_cursor_spans(&mut spans, input.as_str(), input.cursor, add_style);
            lines.push(Line::from(spans));
        }
        _ => lines.push(Line::from(vec![
            Span::styled(" + ", add_style.fg(ACCENT)),
            Span::styled("add a command", add_style.dim()),
        ])),
    }
    lines.push(Line::from(Span::styled(
        " [ done ] ",
        if list.selected == list.done_row() {
            Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
        } else {
            Style::new().bold()
        },
    )));
    frame.render_widget(Paragraph::new(lines), list_area);

    let hint = if list.input.is_some() {
        "Enter save · Esc cancel entry"
    } else {
        "Enter edit · a add · d remove · g global · t terminal · [ done ] saves"
    };
    frame.render_widget(Paragraph::new(Line::from(hint.dim())), hint_area);
}

/// The `setup.copy` / `setup.run` list editor, floating over the Settings
/// form: one row per entry, then an add row and a done row. Same shape as
/// [`draw_open_command_list`] without the per-command global/terminal toggles.
fn draw_string_list(frame: &mut Frame, area: Rect, list: &StringListEditor) {
    let rows = list.rows().clamp(3, 14) as u16;
    let popup = centered(area, 84, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(list.kind.title());
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(list.kind.heading().dim())),
        head_area,
    );

    let row_style = |row: usize| {
        if list.selected == row {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    for (index, item) in list.items.iter().enumerate() {
        let style = row_style(index);
        let mut spans = vec![Span::styled(" ● ", style.fg(Color::Green))];
        match &list.input {
            Some(input) if list.editing_index == Some(index) => {
                push_cursor_spans(&mut spans, input.as_str(), input.cursor, style)
            }
            _ => spans.push(Span::styled(item.clone(), style)),
        }
        lines.push(Line::from(spans));
    }
    let add_style = row_style(list.add_row());
    match &list.input {
        Some(input) if list.editing_index.is_none() => {
            let mut spans = vec![Span::styled(" + ", add_style.fg(ACCENT))];
            push_cursor_spans(&mut spans, input.as_str(), input.cursor, add_style);
            lines.push(Line::from(spans));
        }
        _ => lines.push(Line::from(vec![
            Span::styled(" + ", add_style.fg(ACCENT)),
            Span::styled(list.kind.add_label(), add_style.dim()),
        ])),
    }
    lines.push(Line::from(Span::styled(
        " [ done ] ",
        if list.selected == list.done_row() {
            Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
        } else {
            Style::new().bold()
        },
    )));
    frame.render_widget(Paragraph::new(lines), list_area);

    let hint = if list.input.is_some() {
        "Enter save · Esc cancel entry"
    } else {
        "Enter edit · a add · d remove · [ done ] saves"
    };
    frame.render_widget(Paragraph::new(Line::from(hint.dim())), hint_area);
}

/// Rows the commit dialog's body box occupies, borders included.
const COMMIT_BODY_ROWS: u16 = 7;

/// Commit dialog: a scrollable checklist of the files that will be committed
/// (all ticked by default) above a clearly labelled subject input and an
/// optional multi-line body. Focus cycles through the three panes.
#[allow(clippy::too_many_arguments)]
fn draw_commit(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    files: &[StatusEntry],
    marked: &[bool],
    cursor: usize,
    input: &super::app::TextInput,
    body: &super::app::TextArea,
    focus: &CommitFocus,
    loading: bool,
) -> Option<RowList> {
    /// The four single-row fields: the two labels, the subject, and the hint.
    const CHROME: u16 = 4;
    // While the file list is loading it is one placeholder row, so the dialog
    // does not jump from full height to empty and back as the load lands.
    let list_rows = (files.len() as u16).clamp(1, 10);
    let popup = centered(area, 72, list_rows + 1 + CHROME + COMMIT_BODY_ROWS + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(dialog_panel(format!("commit · {name}")), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    // A short terminal clamps the popup, so budget the rows explicitly rather
    // than letting fixed constraints push the lower fields off: the body box
    // gives way first, then the file list (which scrolls to its cursor).
    let body_rows = inner
        .height
        .saturating_sub(CHROME + 2)
        .min(COMMIT_BODY_ROWS);
    let files_rows = inner.height.saturating_sub(CHROME + body_rows);
    let [
        files_area,
        label_area,
        prompt_area,
        body_label_area,
        body_area,
        hint_area,
    ] = Layout::vertical([
        Constraint::Length(files_rows),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(body_rows),
        Constraint::Length(1),
    ])
    .areas(inner);

    let files_focused = *focus == CommitFocus::Files;
    // `git status` runs off-thread, so the list is empty for the first frames
    // of a large changeset. Say so rather than showing an empty pane.
    if loading {
        frame.render_widget(
            Paragraph::new(Line::from("reading changes…".dim())),
            files_area,
        );
        draw_commit_fields(
            frame,
            [
                label_area,
                prompt_area,
                body_label_area,
                body_area,
                hint_area,
            ],
            input,
            body,
            focus,
            marked,
            files.len(),
            loading,
        );
        // Nothing to click while the list is a placeholder.
        return None;
    }
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let checked = marked.get(i).copied().unwrap_or(false);
            let check = if checked {
                Span::styled("[x] ", Style::new().fg(theme::SUCCESS))
            } else {
                Span::styled("[ ] ", Style::new().dim())
            };
            ListItem::new(Line::from(vec![
                check,
                Span::styled(format!("{:<3}", f.code.trim()), status_style(&f.code)),
                Span::raw(f.path.clone()),
            ]))
        })
        .collect();
    let mut list = List::new(items);
    if files_focused {
        list = list
            .highlight_style(Style::new().bg(SELECTION_BG).bold())
            .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    } else {
        list = list.highlight_symbol("  ");
    }
    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(list, files_area, &mut state);
    // The popup shows at most 10 rows; ListState scrolls so the cursor file
    // stays on screen and clicks map onto that window.
    let list_hit = RowList {
        inner: files_area,
        header: 0,
        offset: state.offset(),
        len: files.len(),
    };

    draw_commit_fields(
        frame,
        [
            label_area,
            prompt_area,
            body_label_area,
            body_area,
            hint_area,
        ],
        input,
        body,
        focus,
        marked,
        files.len(),
        loading,
    );
    Some(list_hit)
}

/// The commit dialog's lower half: the subject label and field, the body label
/// and box, and the hint line. Split out so the loading state can draw them
/// while the file list above is still a placeholder, which is what lets the
/// user start typing a message before `git status` returns.
#[allow(clippy::too_many_arguments)]
fn draw_commit_fields(
    frame: &mut Frame,
    areas: [Rect; 5],
    input: &super::app::TextInput,
    body: &super::app::TextArea,
    focus: &CommitFocus,
    marked: &[bool],
    total_files: usize,
    loading: bool,
) {
    let [
        label_area,
        prompt_area,
        body_label_area,
        body_area,
        hint_area,
    ] = areas;
    // Labels make it obvious which field is which, and which one is live.
    let label_style = |on: bool| {
        if on {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new().dim()
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            "Commit message:",
            label_style(*focus == CommitFocus::Message),
        )),
        label_area,
    );
    // Windowed so a subject longer than the dialog scrolls with the cursor
    // instead of running off the right edge.
    frame.render_widget(
        Paragraph::new(prompt_line_windowed(
            input.as_str(),
            input.cursor,
            prompt_area.width,
        )),
        prompt_area,
    );

    let body_focused = *focus == CommitFocus::Body;
    frame.render_widget(
        Paragraph::new(Line::styled("Body (optional):", label_style(body_focused))),
        body_label_area,
    );
    draw_commit_body(frame, body_area, body, body_focused);

    let selected_count = marked.iter().filter(|m| **m).count();
    let counts = if loading {
        "reading changes…".to_string()
    } else {
        format!(
            "{selected_count}/{total_files} file{}",
            if total_files == 1 { "" } else { "s" }
        )
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("{counts} · Tab switches pane · Space toggles · ^S commits"),
            Style::new().dim(),
        )),
        hint_area,
    );
}

/// The commit dialog's body box: a bordered multi-line field that scrolls
/// vertically to keep the cursor row on screen, with the cursor row itself
/// windowed horizontally the same way the subject line is.
fn draw_commit_body(frame: &mut Frame, area: Rect, body: &super::app::TextArea, focused: bool) {
    let block = Block::bordered().border_style(if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(theme::BORDER)
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = inner.height as usize;
    // Vertical window: scroll only once the cursor passes the last row.
    let (top, _) = cursor_window(body.lines.len(), body.row, rows.max(1));
    let text: Vec<Line> = body
        .lines
        .iter()
        .enumerate()
        .skip(top)
        .take(rows)
        .map(|(i, line)| {
            if focused && i == body.row {
                cursor_line_windowed(line, body.col, inner.width)
            } else {
                Line::raw(line.clone())
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(text), inner);
}

/// Colors a porcelain status code: green when staged, red when only in the
/// working tree, yellow when untracked.
fn status_style(code: &str) -> Style {
    match code.chars().next() {
        Some('?') => Style::new().fg(theme::WARNING),
        Some(' ') | None => Style::new().fg(theme::DANGER),
        _ => Style::new().fg(theme::SUCCESS),
    }
}

/// The Stash tab: a full-width table of the repo's stash entries (stashes are
/// repo-global, not tied to one worktree). The message/drop prompts are
/// modals drawn over it; apply/pop open a destination picker first.
fn draw_stash_tab(frame: &mut Frame, area: Rect, app: &App) -> Option<RowList> {
    let block = panel(format!("stash · shared · apply into {}", app.stash_name));
    let inner = block.inner(area);
    if app.stash_entries.is_empty() {
        let para = Paragraph::new(Line::from(
            "no stashes — s stashes the current changes".dim(),
        ))
        .block(block);
        frame.render_widget(para, area);
        return None;
    }
    let rows: Vec<Row> = app
        .stash_entries
        .iter()
        .map(|e| {
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    format!("stash@{{{}}}", e.index),
                    Style::new().fg(ACCENT),
                ))),
                Cell::from(Line::from(Span::styled(
                    e.message.clone(),
                    Style::new().bold(),
                ))),
                Cell::from(Line::from(Span::styled(
                    e.branch.clone(),
                    Style::new().dim(),
                ))),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(24),
        ],
    )
    .header(Row::new(["#", "MESSAGE", "BRANCH"]).style(Style::new().dim().bold()))
    .block(block)
    .row_highlight_style(Style::new().bg(SELECTION_BG).bold())
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
    let mut state = TableState::default().with_selected(Some(app.stash_selected));
    frame.render_stateful_widget(table, area, &mut state);
    // Header takes the first inner row; the rest is the visible window.
    let visible = inner.height.saturating_sub(1) as usize;
    let total = app.stash_entries.len();
    draw_scroll_arrows(frame, area, state.offset(), visible, total);
    Some(RowList {
        inner,
        header: 1,
        offset: state.offset(),
        len: total,
    })
}

/// Top-of-main tab bar: the active tab in accent, the other dimmed, with a
/// reminder that Tab switches between them.
fn draw_tab_bar(frame: &mut Frame, area: Rect, app: &mut App) {
    let tab_span = |label: String, active: bool, warn: bool| {
        if active {
            let bg = if warn { theme::WARNING } else { ACCENT };
            Span::styled(
                format!(" {label} "),
                Style::new().fg(Color::Black).bg(bg).bold(),
            )
        } else if warn {
            Span::styled(format!(" {label} "), Style::new().fg(theme::WARNING).bold())
        } else {
            Span::styled(format!(" {label} "), Style::new().fg(BORDER))
        }
    };
    let mut spans = Vec::new();
    // Walk the labels left to right, recording each one's screen rect so a
    // click can be mapped back to its tab.
    let mut x = area.x;
    let mut hits = Vec::new();
    for tab in Tab::ALL {
        // The three-panel layout folds the Changes tab into the Worktrees tab,
        // so it isn't offered here either.
        if app.tab_hidden(tab) {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
            x += 1;
        }
        // The Changes tab carries the conflict warning: while a resolver is up
        // it *is* that tab, so this is the standing pointer back into a
        // half-finished merge from wherever the user wandered off to.
        let warn = tab == Tab::Changes && app.resolver.is_some();
        let glyph = if warn { "⚠" } else { tab.glyph() };
        let span = tab_span(format!("{} {}", glyph, tab.title()), app.tab == tab, warn);
        let width = span.width() as u16;
        if x < area.x + area.width {
            let rect = Rect {
                x,
                y: area.y,
                width: width.min(area.x + area.width - x),
                height: 1,
            };
            hits.push((rect, tab));
        }
        x += width;
        spans.push(span);
    }
    spans.push(Span::styled("   ⇥/⇧⇥ switch", Style::new().dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    app.tab_hits = hits;
}

/// Width of the branches table's CHECKED OUT column, shared by the layout
/// constraint and by the path-truncation budget derived from it.
const BRANCH_CHECKOUT_W: u16 = 24;

/// The Branches tab: a full-width table of branches in two labelled groups,
/// local first and remote-only second, with the inline new-branch and
/// confirm-delete popups floating on top. Returns the clickable row list
/// (suppressed while a popup is up); its indices are display rows, so
/// `App::select_branch_row` maps a click back to a branch.
fn draw_branches(frame: &mut Frame, area: Rect, app: &App) -> Option<RowList> {
    // The title carries the two things the list alone can't say: that a
    // background reload is running, and that archived branches are being held
    // back (or are currently shown).
    let mut title = "branches".to_string();
    if app.show_archived {
        title.push_str(" · archived shown");
    } else {
        let hidden = app.archived_hidden_count();
        if hidden > 0 {
            title.push_str(&format!(" · {hidden} archived hidden (v)"));
        }
    }
    if app.branches_loading() && !app.branches.is_empty() {
        title.push_str(" · refreshing…");
    }
    let block = panel(title);
    let inner = block.inner(area);
    // The very first load has no cached list to fall back on, so show a
    // placeholder instead of an empty table while it fetches in the
    // background.
    if app.branches_first_load() {
        frame.render_widget(
            Paragraph::new(Line::from("loading branches…".dim())),
            block.inner(area),
        );
        frame.render_widget(block, area);
        return None;
    }
    let display_rows = branch_display_rows(&app.branches);
    let rows: Vec<Row> = display_rows
        .iter()
        .map(|row| {
            let index = match row {
                // A group heading names the group and says how the branches
                // under it differ, so "local" vs "remote" doesn't rest on the
                // checked-out column alone. A table cell can't span columns, so
                // the label and its note go in the first two cells and each is
                // kept short enough for that column's width.
                BranchRow::Header(label) => {
                    let note = if *label == "LOCAL BRANCHES" {
                        "in this repo"
                    } else {
                        "on a remote only"
                    };
                    return Row::new(vec![
                        Cell::from(Line::styled(*label, Style::new().fg(ACCENT).bold())),
                        Cell::from(Line::styled(note, Style::new().dim())),
                    ]);
                }
                BranchRow::Branch(index) => *index,
            };
            let b = &app.branches[index];
            // An archived branch is only visible with `v` on, so it is dimmed
            // and marked to keep it distinct from the branches always listed.
            let name = if b.archived {
                Span::styled(
                    format!("{} (archived)", b.name),
                    Style::new().dim().italic(),
                )
            } else {
                Span::styled(b.name.clone(), Style::new().bold())
            };
            // The column is fixed-width, so the path is trimmed from the
            // front here rather than clipped from the back by the table: a
            // worktree is identified by the tail of its path. The full value
            // is on the detail line under the table.
            let checkout = match (&b.checked_out_path, &b.remote) {
                (Some(p), _) => Span::styled(
                    format!("● {}", truncate_start(p, BRANCH_CHECKOUT_W as usize - 2)),
                    Style::new().fg(theme::SUCCESS),
                ),
                (None, Some(remote)) => Span::styled(
                    format!(
                        "☁ {}",
                        truncate_middle(remote, BRANCH_CHECKOUT_W as usize - 2)
                    ),
                    Style::new().fg(theme::INFO).dim(),
                ),
                (None, None) => Span::styled("–".to_string(), Style::new().dim()),
            };
            let track = if b.upstream.is_some() {
                Span::styled(
                    format!("↑{} ↓{}", b.ahead, b.behind),
                    Style::new().fg(ACCENT),
                )
            } else {
                Span::styled("no upstream".to_string(), Style::new().dim())
            };
            let flag_spans = status_flag_spans(&b.flag_labels());
            let last = Span::styled(format!("{}  {}", b.date, b.subject), Style::new().dim());
            Row::new(vec![
                Cell::from(Line::from(name)),
                Cell::from(Line::from(checkout)),
                Cell::from(Line::from(track)),
                Cell::from(Line::from(flag_spans)),
                Cell::from(Line::from(last)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(BRANCH_CHECKOUT_W),
            Constraint::Length(14),
            Constraint::Length(28),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["BRANCH", "CHECKED OUT", "UPSTREAM", "FLAGS", "LAST COMMIT"])
            .style(Style::new().dim().bold()),
    )
    .row_highlight_style(Style::new().bg(SELECTION_BG).bold())
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
    // The last inner row belongs to the detail line, so the block is drawn on
    // its own and the table gets the rows above it.
    frame.render_widget(block, area);
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let mut state = TableState::default()
        .with_selected(Some(branch_row_of(&display_rows, app.branch_selected)));
    frame.render_stateful_widget(table, table_area, &mut state);
    // The detail line already took a row off `inner`, so the visible window is
    // `table_area` minus its own header. Rows are counted over `display_rows`
    // (group headings included), matching the `RowList` returned below.
    let visible = table_area.height.saturating_sub(1) as usize;
    draw_scroll_arrows(frame, area, state.offset(), visible, display_rows.len());
    if let Some(b) = app.branches.get(app.branch_selected) {
        frame.render_widget(
            Paragraph::new(branch_detail_line(b, detail_area.width as usize)),
            detail_area,
        );
    }

    // The create/rename/delete prompts are modals now, drawn over this list.
    Some(RowList {
        inner: table_area,
        header: 1,
        offset: state.offset(),
        len: display_rows.len(),
    })
}

/// The line under the branches table: the selected branch's full name beside
/// the full path of the worktree that has it checked out. The table's columns
/// are fixed-width and elide both, so this is where the untruncated values
/// live. When even this line is too narrow the path gives way first (from the
/// front, down to an eight-character floor), and only past that is the name
/// itself elided from the middle.
fn branch_detail_line(b: &BranchListItem, width: usize) -> Line<'static> {
    const PREFIX: &str = "⎇ ";
    const GAP: &str = "   ";
    let (marker, value, style) = match (&b.checked_out_path, &b.remote) {
        (Some(p), _) => ("● ", p.clone(), Style::new().fg(theme::SUCCESS)),
        (None, Some(remote)) => (
            "☁ ",
            format!("on {remote} only"),
            Style::new().fg(theme::INFO),
        ),
        (None, None) => ("", "not checked out".to_string(), Style::new().dim()),
    };
    let fixed = PREFIX.chars().count() + GAP.chars().count() + marker.chars().count();
    let avail = width.saturating_sub(fixed);
    let name_w = b.name.chars().count();
    let value_w = value.chars().count();
    let (name, value) = if name_w + value_w <= avail {
        (b.name.clone(), value)
    } else {
        let floor = 8.min(avail);
        let value_budget = value_w.min(avail.saturating_sub(name_w)).max(floor);
        let name_budget = avail.saturating_sub(value_budget);
        (
            truncate_middle(&b.name, name_budget),
            truncate_start(&value, value_budget),
        )
    };
    Line::from(vec![
        Span::styled(PREFIX, Style::new().fg(ACCENT)),
        Span::styled(name, Style::new().bold()),
        Span::raw(GAP),
        Span::styled(marker, style),
        Span::styled(value, style),
    ])
}

/// The switch-branch picker: a type-to-filter prompt over a centered list of
/// branches the selected worktree can switch onto (those not checked out
/// anywhere else, plus remote-only branches).
fn draw_switch(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    branches: &[CheckoutCandidate],
    filter: &TextInput,
    selected: usize,
) -> Option<RowList> {
    let matches = filtered_candidates(branches, filter.as_str());
    // +2 rows: the filter prompt and the hint line below the list.
    let rows = matches.len().clamp(1, 12) as u16;
    let popup = centered(area, 52, rows + 4);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(format!("switch '{name}' to branch"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [filter_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(prompt_line_at(filter.as_str(), filter.cursor)),
        filter_area,
    );
    let hit = if matches.is_empty() {
        // Nothing matches, but Enter creates the typed name as a new branch, so
        // say so rather than leaving the picker looking like a dead end.
        let typed = filter.as_str().trim();
        let empty = if typed.is_empty() {
            "no other branches · type a name to create a branch".to_string()
        } else {
            format!("no match · Enter creates & switches to '{typed}'")
        };
        frame.render_widget(
            Paragraph::new(Line::styled(empty, Style::new().dim())),
            list_area,
        );
        None
    } else {
        let items: Vec<ListItem> = matches
            .iter()
            .map(|&idx| {
                let candidate = &branches[idx];
                let mut spans = vec![
                    Span::styled("⎇ ", Style::new().fg(ACCENT)),
                    Span::raw(candidate.branch.clone()),
                ];
                // Flag remote-only branches, since switching onto one checks it
                // out as a new local tracking branch.
                if let Some(remote) = &candidate.remote {
                    spans.push(Span::styled(
                        format!("  ({remote})"),
                        Style::new().fg(ACCENT).dim(),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(Style::new().bg(SELECTION_BG).bold())
            .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
        let mut state = ListState::default().with_selected(Some(selected.min(matches.len() - 1)));
        frame.render_stateful_widget(list, list_area, &mut state);
        Some(RowList {
            inner: list_area,
            header: 0,
            offset: state.offset(),
            len: matches.len(),
        })
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            "type to filter or name a new branch · ↑/↓ pick · Enter switch/create · Esc clear/cancel",
            Style::new().dim(),
        )),
        hint_area,
    );
    hit
}

/// Widest graph column any row is padded out to. Past this one deep merge fan
/// would push every subject off the right-hand edge, so the column stops growing
/// and the rare wider row overflows it instead.
const GRAPH_WIDTH_MAX: usize = 24;

/// Widest graph art across `rows`, in characters, so every row can be padded to
/// one width and the commit fields start in the same column on every line.
/// git's art is only as wide as each line's own topology needs (`*` on the
/// trunk, `| | *` three lanes over), which left the hashes and subjects stepping
/// in and out down the panel. Trailing spaces don't count: git pads a merge line
/// out to make room for the parents it is about to fan, and widening the column
/// for art nothing is drawn in would indent the whole log for nothing.
fn graph_width(rows: &[GraphLine]) -> usize {
    rows.iter()
        .map(|r| r.graph.trim_end().chars().count())
        .max()
        .unwrap_or(0)
        .min(GRAPH_WIDTH_MAX)
}

/// Color of graph column `col`. git spaces lanes two columns apart, so halving
/// the column index gives each lane one stable color.
fn lane_color_at(col: usize) -> Color {
    GRAPH_COLORS[(col / 2) % GRAPH_COLORS.len()]
}

/// Color of the lane a commit's node (`*`) is drawn in, so its hash can be
/// tinted to match the branch line it hangs off. `None` for a row with no node:
/// flat mode carries no art at all, so there is no lane to borrow a color from.
fn lane_color(graph: &str) -> Option<Color> {
    graph.chars().position(|c| c == '*').map(lane_color_at)
}

/// Renders one `git log --graph` art prefix, translating git's ASCII (`* | / \`)
/// into box-drawing characters and coloring each column by its lane. Padded out
/// to `width` (from `graph_width`) plus one separating space, so whatever comes
/// after it starts in the same column on every row. Empty in flat mode, where
/// rows carry no art and `width` is 0.
fn graph_spans(graph: &str, width: usize) -> Vec<Span<'static>> {
    let art = graph.trim_end();
    if art.is_empty() && width == 0 {
        return Vec::new();
    }
    let mut spans: Vec<Span<'static>> = art
        .chars()
        .enumerate()
        .map(|(col, c)| {
            let ch = match c {
                '*' => '●',
                '|' => '│',
                '/' => '╱',
                '\\' => '╲',
                '_' | '-' => '─',
                other => other,
            };
            Span::styled(ch.to_string(), Style::new().fg(lane_color_at(col)))
        })
        .collect();
    let drawn = art.chars().count();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(drawn) + 1)));
    spans
}

/// Ref decorations next to a commit (`(HEAD -> main, origin/main)`), colored the
/// way git's own log colors them: cyan HEAD, green local branches, red remotes,
/// yellow tags. Empty for the commits nothing points at.
fn ref_spans(refs: &[String]) -> Vec<Span<'static>> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::styled("(", Style::new().dim())];
    for (i, r) in refs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(", ", Style::new().dim()));
        }
        let color = if r.starts_with("HEAD") {
            ACCENT
        } else if r.starts_with("tag:") {
            Color::Yellow
        } else if r.contains('/') {
            Color::Red
        } else {
            Color::Green
        };
        spans.push(Span::styled(r.clone(), Style::new().fg(color).bold()));
    }
    spans.push(Span::styled(") ", Style::new().dim()));
    spans
}

/// The commit fields (hash, refs, subject, author/date) drawn after the graph.
/// `hash_width` abbreviates the full hashes the branch view stores. `lane` is
/// the color of the graph lane this commit's node sits in, so the hash reads as
/// belonging to the same branch line as the dot beside it; flat mode has no
/// lanes and falls back to a plain yellow hash.
fn commit_spans(
    e: &crate::git::LogEntry,
    hash_width: usize,
    lane: Option<Color>,
) -> Vec<Span<'static>> {
    let short = &e.hash[..e.hash.len().min(hash_width)];
    let mut spans = vec![Span::styled(
        format!("{short} "),
        Style::new().fg(lane.unwrap_or(Color::Yellow)),
    )];
    spans.extend(ref_spans(&e.refs));
    spans.push(Span::raw(format!("{}  ", e.subject)));
    spans.push(Span::styled(
        format!("{} · {}", e.author, e.date),
        Style::new().dim(),
    ));
    spans
}

/// Base style for a commit row: the branch's own commits (ahead of the base it
/// was created off) get a bold band so they read apart from the history the
/// branch merely inherited. Ratatui patches `highlight_style` over this, so the
/// cursor row still wins on a marked commit; the dim author/date span in
/// `commit_spans` likewise keeps its own styling.
fn commit_row_style(own: bool) -> Style {
    if own {
        Style::new().bold().bg(theme::OWN_COMMIT_BG)
    } else {
        Style::new()
    }
}

/// Suffix for a commit panel's title naming how many of the rows are the
/// branch's own work, so the band has a legend. Empty when none are.
fn own_commit_note(own: &[bool]) -> String {
    let count = own.iter().filter(|o| **o).count();
    if count == 0 {
        String::new()
    } else {
        format!(" · {count} on this branch")
    }
}

/// Scrollable commit log, styled like the diff view. In tree mode rows carry
/// graph art and some hold art alone; in flat mode every row is a commit.
fn draw_log(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    rows: &[GraphLine],
    own: &[bool],
    selected: usize,
    mode: LogMode,
) -> Option<RowList> {
    let block = panel(format!(
        "log · {name} · {}{}",
        mode.label(),
        own_commit_note(own)
    ));
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let graph_w = graph_width(rows);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = graph_spans(&row.graph, graph_w);
            if let Some(e) = &row.entry {
                spans.extend(commit_spans(e, usize::MAX, lane_color(&row.graph)));
            }
            ListItem::new(Line::from(spans))
                .style(commit_row_style(own.get(i).copied().unwrap_or(false)))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    Some(RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    })
}

/// Read-only browser for a single commit's changes: the changed files (tree or
/// flat) on the left, the selected file's diff on the right. A trimmed-down
/// twin of `draw_diff` with no commit/stash/revert affordances.
#[allow(clippy::too_many_arguments)]
fn draw_commit_diff(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    files: &[StatusEntry],
    rows: &[DiffRow],
    selected: usize,
    content: &str,
    loading_new: bool,
    scroll: u16,
    h_scroll: u16,
    // Whether the diff pane draws its line-number gutter (`diff_line_numbers`).
    numbered: bool,
) -> Option<RowList> {
    if files.is_empty() {
        let para = Paragraph::new(Line::from("this commit changed no files".dim()))
            .block(panel(format!("commit · {label}")));
        frame.render_widget(para, area);
        return None;
    }

    let [list_area, diff_area] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);

    // Left: the changed files as a folder tree or flat list (no checkboxes).
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            DiffRow::Folder {
                label,
                depth,
                collapsed,
                ..
            } => {
                let indent = "  ".repeat(*depth);
                let arrow = if *collapsed { "▸ " } else { "▾ " };
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(arrow, Style::new().fg(ACCENT)),
                    Span::styled(format!("{label}/"), Style::new().fg(ACCENT).bold()),
                ]))
            }
            DiffRow::File {
                index,
                label,
                depth,
            } => {
                let indent = "  ".repeat(*depth);
                let code = files.get(*index).map(|f| f.code.trim()).unwrap_or("");
                let style = files
                    .get(*index)
                    .map(|f| status_style(&f.code))
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(format!("{code:<2} "), style),
                    Span::raw(label.clone()),
                ]))
            }
        })
        .collect();
    let block = panel(format!("files · {label}"));
    let inner = block.inner(list_area);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    let list_hit = RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    };

    // Right: the diff of the highlighted file (or a folder summary).
    let (title, lines): (String, Vec<Line>) = match rows.get(selected) {
        Some(DiffRow::Folder { prefix, .. }) => {
            let count = files
                .iter()
                .filter(|f| f.path.starts_with(prefix.as_str()))
                .count();
            (
                format!("folder · {prefix}"),
                vec![Line::from(
                    format!("{count} changed file(s) under {prefix}").dim(),
                )],
            )
        }
        _ => {
            let path = current_diff_path(rows, files, selected);
            let lines = if loading_new {
                vec![Line::from("loading diff…".dim())]
            } else if content.is_empty() {
                vec![Line::from("no textual diff (binary or empty)".dim())]
            } else {
                diff_lines_with_gutter(path, content, numbered)
            };
            (format!("diff · {path}"), lines)
        }
    };
    let total = lines.len();
    let para = Paragraph::new(lines)
        .block(panel(title))
        .scroll((scroll, h_scroll));
    frame.render_widget(para, diff_area);
    let mut sb_state = ScrollbarState::new(total.saturating_sub(diff_area.height as usize))
        .position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::new().fg(BORDER))
            .thumb_style(Style::new().fg(ACCENT)),
        diff_area,
        &mut sb_state,
    );
    Some(list_hit)
}

/// Commit history filling the bottom of the three-panel Worktrees layout when
/// the selected worktree is clean. Same row content as `draw_log` /
/// `View::BranchCommits`, with the focus dimming used by the files panel.
#[allow(clippy::too_many_arguments)]
fn draw_worktree_commits(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    rows: &[GraphLine],
    own: &[bool],
    selected: usize,
    mode: LogMode,
    focused: bool,
    // Click-to-copy regions to add the title's branch name to, so the name
    // sitting in the border can be lifted out without retyping it.
    copy_hits: &mut Vec<CopyHit>,
) -> Option<RowList> {
    let block = focus_panel(
        format!(
            "commits · {branch} · {}{}",
            mode.label(),
            own_commit_note(own)
        ),
        focused,
    );
    if !branch.is_empty() {
        // `focus_panel` draws its title one cell in from the border with a
        // leading space, and "commits · " runs ahead of the name.
        const TITLE_X: u16 = 2;
        let prefix = "commits · ".chars().count() as u16;
        let start = TITLE_X + prefix;
        let room = area.width.saturating_sub(start + 1);
        let width = (branch.chars().count() as u16).min(room);
        if width > 0 {
            copy_hits.push(CopyHit {
                rect: Rect {
                    x: area.x + start,
                    y: area.y,
                    width,
                    height: 1,
                },
                value: branch.to_string(),
                label: "branch",
                row: None,
            });
        }
    }
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let graph_w = graph_width(rows);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = graph_spans(&row.graph, graph_w);
            if let Some(e) = &row.entry {
                spans.extend(commit_spans(e, 9, lane_color(&row.graph)));
            }
            ListItem::new(Line::from(spans))
                .style(commit_row_style(own.get(i).copied().unwrap_or(false)))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style(focused))
        .highlight_symbol(Span::styled("▌", Style::new().fg(cursor_color(focused))));
    let mut state = ListState::default().with_selected(Some(selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    Some(RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    })
}

/// A branch's commit history with a commit checkbox on each row. Marked commits
/// (or the one under the cursor) are cherry-picked into a worktree via Enter.
#[allow(clippy::too_many_arguments)]
fn draw_branch_commits(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    rows: &[GraphLine],
    marked: &[bool],
    own: &[bool],
    selected: usize,
    mode: LogMode,
) -> Option<RowList> {
    let block = panel(format!(
        "commits · {branch} · {}{}",
        mode.label(),
        own_commit_note(own)
    ));
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let graph_w = graph_width(rows);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let Some(e) = &row.entry else {
                // An art-only row has no checkbox; pad past that column so its
                // graph still lines up with the commits above and below.
                let mut spans = vec![Span::raw("    ")];
                spans.extend(graph_spans(&row.graph, graph_w));
                return ListItem::new(Line::from(spans));
            };
            let style = commit_row_style(own.get(i).copied().unwrap_or(false));
            let checked = marked.get(i).copied().unwrap_or(false);
            let mut spans = vec![if checked {
                Span::styled("[x] ", Style::new().fg(theme::SUCCESS))
            } else {
                Span::styled("[ ] ", Style::new().dim())
            }];
            spans.extend(graph_spans(&row.graph, graph_w));
            // Full hashes are stored for cherry-pick; show an abbreviated form.
            spans.extend(commit_spans(e, 9, lane_color(&row.graph)));
            ListItem::new(Line::from(spans)).style(style)
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    Some(RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    })
}

/// The cherry-pick flow overlay: first a worktree picker (`mode` is None), then
/// a commit-vs-load-only choice (`mode` is Some).
fn draw_cherry_pick(
    frame: &mut Frame,
    area: Rect,
    source_branch: &str,
    summaries: &[String],
    targets: &[CherryTarget],
    selected: usize,
    mode: Option<usize>,
) -> Option<RowList> {
    let n = summaries.len();
    let plural = if n == 1 { "commit" } else { "commits" };
    match mode {
        // Commit vs load-only.
        Some(m) => {
            let popup = centered(area, 60, 7);
            frame.render_widget(Clear, popup);
            let option = |sel: bool, label: &str| -> Line<'static> {
                let marker = if sel { "▌ ● " } else { "  ○ " };
                let style = if sel {
                    Style::new().bg(SELECTION_BG).bold()
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::styled(marker.to_string(), style.fg(ACCENT)),
                    Span::styled(label.to_string(), style),
                ])
            };
            let lines = vec![
                Line::from(format!("apply {n} {plural} into the worktree:").dim()),
                Line::from(""),
                option(m == 0, "Commit directly (keep original messages)"),
                option(m == 1, "Load changes only (review, then commit)"),
                Line::from(""),
                Line::from("↑/↓ choose · Enter confirm · Esc back".dim()),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(dialog_panel("cherry-pick mode")),
                popup,
            );
            None
        }
        // Worktree picker.
        None => {
            let rows = targets.len().clamp(1, 12) as u16;
            let popup = centered(area, 60, rows + 5);
            frame.render_widget(Clear, popup);
            let block = dialog_panel(format!("cherry-pick {n} {plural} from '{source_branch}'"));
            frame.render_widget(&block, popup);
            let inner = block.inner(popup);
            let [head_area, list_area, hint_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner);
            frame.render_widget(
                Paragraph::new(Line::from("into which worktree?".dim())),
                head_area,
            );
            let items: Vec<ListItem> = targets
                .iter()
                .map(|t| {
                    let branch = match &t.branch {
                        Some(b) => format!(" ({b})"),
                        None => " (detached)".to_string(),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled("● ", Style::new().fg(Color::Green)),
                        Span::raw(t.name.clone()),
                        Span::styled(branch, Style::new().dim()),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .highlight_style(Style::new().bg(SELECTION_BG).bold())
                .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
            let mut state =
                ListState::default().with_selected(Some(selected.min(targets.len().max(1) - 1)));
            frame.render_stateful_widget(list, list_area, &mut state);
            frame.render_widget(
                Paragraph::new(Line::from(
                    "↑/↓ pick · Enter choose mode · Esc cancel".dim(),
                )),
                hint_area,
            );
            (!targets.is_empty()).then_some(RowList {
                inner: list_area,
                header: 0,
                offset: state.offset(),
                len: targets.len(),
            })
        }
    }
}

/// The merge picker overlay: choose which worktree to merge the selected
/// branch into. Mirrors the cherry-pick worktree picker.
fn draw_merge_pick(
    frame: &mut Frame,
    area: Rect,
    source_branch: &str,
    targets: &[CherryTarget],
    selected: usize,
) -> Option<RowList> {
    draw_worktree_pick(
        frame,
        area,
        &format!("merge '{source_branch}' into worktree"),
        "into which worktree?",
        "merge",
        targets,
        selected,
    )
}

/// The rebase picker overlay: choose which worktree to replay onto the selected
/// branch. Mirrors the merge picker, but the branch is the destination rather
/// than the source, so the wording is reversed.
fn draw_rebase_pick(
    frame: &mut Frame,
    area: Rect,
    onto_branch: &str,
    targets: &[CherryTarget],
    selected: usize,
) -> Option<RowList> {
    draw_worktree_pick(
        frame,
        area,
        &format!("rebase a worktree onto '{onto_branch}'"),
        "which worktree should be replayed?",
        "rebase",
        targets,
        selected,
    )
}

/// Shared body of the merge and rebase pickers: a list of worktrees with the
/// branch each has checked out, titled and captioned by the caller.
fn draw_worktree_pick(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    head: &str,
    verb: &str,
    targets: &[CherryTarget],
    selected: usize,
) -> Option<RowList> {
    let rows = targets.len().clamp(1, 12) as u16;
    let popup = centered(area, 60, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(title.to_string());
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(head.to_string().dim())),
        head_area,
    );
    let items: Vec<ListItem> = targets
        .iter()
        .map(|t| {
            let branch = match &t.branch {
                Some(b) => format!(" ({b})"),
                None => " (detached)".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(Color::Green)),
                Span::raw(t.name.clone()),
                Span::styled(branch, Style::new().dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state =
        ListState::default().with_selected(Some(selected.min(targets.len().max(1) - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(
            format!("↑/↓ pick · Enter {verb} · Esc cancel").dim(),
        )),
        hint_area,
    );
    (!targets.is_empty()).then_some(RowList {
        inner: list_area,
        header: 0,
        offset: state.offset(),
        len: targets.len(),
    })
}

/// The move-changes picker overlay: choose which worktree to move the
/// selected worktree's uncommitted changes into. Mirrors the merge picker.
fn draw_move_changes_pick(
    frame: &mut Frame,
    area: Rect,
    from: &str,
    targets: &[CherryTarget],
    selected: usize,
) -> Option<RowList> {
    let rows = targets.len().clamp(1, 12) as u16;
    let popup = centered(area, 60, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(format!("move changes from '{from}' into…"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from("into which worktree?".dim())),
        head_area,
    );
    let items: Vec<ListItem> = targets
        .iter()
        .map(|t| {
            let branch = match &t.branch {
                Some(b) => format!(" ({b})"),
                None => " (detached)".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(Color::Green)),
                Span::raw(t.name.clone()),
                Span::styled(branch, Style::new().dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state =
        ListState::default().with_selected(Some(selected.min(targets.len().max(1) - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(
            "↑/↓ pick · Enter move changes · Esc cancel".dim(),
        )),
        hint_area,
    );
    (!targets.is_empty()).then_some(RowList {
        inner: list_area,
        header: 0,
        offset: state.offset(),
        len: targets.len(),
    })
}

/// The open-command picker: choose which configured command to run against
/// the selected worktree. Rows are the expanded commands, so each one is a
/// preview of exactly what will be executed.
fn draw_open_command_pick(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    vars: &OpenCommandVars<'_>,
    commands: &[OpenCommand],
    selected: usize,
) -> Option<RowList> {
    // Expanded commands carry absolute paths, so this picker is wider than the
    // worktree pickers around it.
    let rows = commands.len().clamp(1, 12) as u16;
    let popup = centered(area, 80, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(format!("open '{name}' with…"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(
            "this is what will run · ▶ closes wtm and runs in this terminal".dim(),
        )),
        head_area,
    );
    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            // The mode marker leads the row so a command that will close wtm
            // says so before the (long, absolute-path) command itself.
            let marker = if cmd.mode == CommandMode::Terminal {
                Span::styled("▶ ", Style::new().fg(theme::WARNING))
            } else {
                Span::styled("● ", Style::new().fg(Color::Green))
            };
            ListItem::new(Line::from(vec![
                marker,
                Span::raw(expand_open_command(&cmd.command, vars)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state =
        ListState::default().with_selected(Some(selected.min(commands.len().max(1) - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(Line::from("↑/↓ pick · Enter run · Esc cancel".dim())),
        hint_area,
    );
    (!commands.is_empty()).then_some(RowList {
        inner: list_area,
        header: 0,
        offset: state.offset(),
        len: commands.len(),
    })
}

/// The stash apply/pop destination picker: stashes are repo-global, so
/// choosing where to apply one is a worktree picker like cherry-pick/merge.
fn draw_stash_target_pick(
    frame: &mut Frame,
    area: Rect,
    pop: bool,
    label: &str,
    targets: &[CherryTarget],
    selected: usize,
) -> Option<RowList> {
    let verb = if pop { "pop" } else { "apply" };
    let rows = targets.len().clamp(1, 12) as u16;
    let popup = centered(area, 60, rows + 5);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(format!("{verb} {label} into…"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from("into which worktree?".dim())),
        head_area,
    );
    let items: Vec<ListItem> = targets
        .iter()
        .map(|t| {
            let branch = match &t.branch {
                Some(b) => format!(" ({b})"),
                None => " (detached)".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(Color::Green)),
                Span::raw(t.name.clone()),
                Span::styled(branch, Style::new().dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state =
        ListState::default().with_selected(Some(selected.min(targets.len().max(1) - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(
            format!("↑/↓ pick · Enter {verb} · Esc cancel").dim(),
        )),
        hint_area,
    );
    (!targets.is_empty()).then_some(RowList {
        inner: list_area,
        header: 0,
        offset: state.offset(),
        len: targets.len(),
    })
}

/// The upstream picker: a type-to-filter prompt over the repo's
/// remote-tracking refs, with a "stop tracking" row on top when the branch
/// already has an upstream. The branch's current upstream is named in the
/// header, and marked in the list, so setting it reads as a change.
fn draw_upstream_pick(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    current: Option<&str>,
    candidates: &[String],
    filter: &TextInput,
    selected: usize,
) -> Option<RowList> {
    let rows = upstream_rows(candidates, filter.as_str(), current.is_some());
    // +4 rows: the header, the filter prompt, and the hint line below the list.
    let visible = rows.len().clamp(1, 12) as u16;
    let popup = centered(area, 60, visible + 6);
    frame.render_widget(Clear, popup);
    let block = dialog_panel(format!("upstream for '{branch}'"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, filter_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(
            match current {
                Some(up) => format!("currently tracks {up}"),
                None => "not tracking anything yet".to_string(),
            }
            .dim(),
        )),
        head_area,
    );
    frame.render_widget(
        Paragraph::new(prompt_line_at(filter.as_str(), filter.cursor)),
        filter_area,
    );
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "no remote branch matches".to_string(),
                Style::new().dim(),
            )),
            list_area,
        );
        frame.render_widget(
            Paragraph::new(Line::from("type to filter · Esc cancel".dim())),
            hint_area,
        );
        return None;
    }
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            UpstreamRow::Unset => ListItem::new(Line::from(vec![
                Span::styled("✕ ", Style::new().fg(theme::DANGER)),
                Span::raw("stop tracking a remote branch"),
            ])),
            UpstreamRow::Candidate(i) => {
                let name = candidates[*i].as_str();
                let mut spans = vec![
                    Span::styled("☁ ", Style::new().fg(theme::INFO)),
                    Span::raw(name.to_string()),
                ];
                if current == Some(name) {
                    spans.push(Span::styled("  (current)", Style::new().dim()));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(
            "type to filter · ↑/↓ pick · Enter set · Esc cancel".dim(),
        )),
        hint_area,
    );
    Some(RowList {
        inner: list_area,
        header: 0,
        offset: state.offset(),
        len: rows.len(),
    })
}

/// Color used for the "ours" side everywhere in the resolver.
const OURS_COLOR: Color = theme::SUCCESS;
/// Color used for the "theirs" side everywhere in the resolver.
const THEIRS_COLOR: Color = Color::Blue;
/// Color used for a hand-edited (manual) resolution.
const MANUAL_COLOR: Color = theme::WARNING;

/// Short label and color for a hunk's chosen resolution action, shown on the
/// hunk header so the decision reads at a glance.
fn action_label(action: Option<&ResolutionAction>) -> (&'static str, Color) {
    match action {
        None => ("undecided — press o / t / b", theme::WARNING),
        Some(ResolutionAction::KeepOurs) => ("keeping OURS", OURS_COLOR),
        Some(ResolutionAction::KeepTheirs) => ("keeping THEIRS", THEIRS_COLOR),
        Some(ResolutionAction::KeepBoth) => ("keeping BOTH · ours first", Color::Cyan),
        Some(ResolutionAction::KeepBothReversed) => ("keeping BOTH · theirs first", Color::Cyan),
    }
}

/// How one side of a hunk is being treated by the chosen action: kept (with its
/// position when both sides are kept), dropped, or not yet decided.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SideState {
    /// Kept. `Some(n)` numbers it when both sides are kept in order.
    Kept(Option<u8>),
    Dropped,
    Undecided,
}

impl SideState {
    /// Glyph and fixed-width verdict word for the side's header row. The
    /// verdict leads the row so it can never be the part that gets clipped on a
    /// narrow pane.
    fn marks(self) -> (&'static str, &'static str) {
        match self {
            SideState::Kept(None) => ("✓", "keep    "),
            SideState::Kept(Some(1)) => ("✓", "keep 1st"),
            SideState::Kept(Some(_)) => ("✓", "keep 2nd"),
            SideState::Dropped => ("✗", "drop    "),
            SideState::Undecided => ("○", "        "),
        }
    }
}

/// What `action` does to each side of the hunk, as (ours, theirs).
fn side_states(action: Option<&ResolutionAction>) -> (SideState, SideState) {
    match action {
        None => (SideState::Undecided, SideState::Undecided),
        Some(ResolutionAction::KeepOurs) => (SideState::Kept(None), SideState::Dropped),
        Some(ResolutionAction::KeepTheirs) => (SideState::Dropped, SideState::Kept(None)),
        Some(ResolutionAction::KeepBoth) => (SideState::Kept(Some(1)), SideState::Kept(Some(2))),
        Some(ResolutionAction::KeepBothReversed) => {
            (SideState::Kept(Some(2)), SideState::Kept(Some(1)))
        }
    }
}

/// How one side of a hunk is introduced: the box-drawing corner that opens its
/// block, what the side is called, the branch it came from, the key that takes
/// it, its color, and an optional trailing note. Where each side *comes from*
/// lives in the pane's sticky legend instead, so these rows stay short enough
/// not to clip on a narrow pane.
struct SideView<'a> {
    corner: &'a str,
    side: &'a str,
    label: &'a str,
    key: char,
    color: Color,
    note: &'a str,
}

/// One labelled side of a hunk: a header row naming the side, its branch, and
/// whether it is being kept, followed by its lines inside a colored gutter. A
/// dropped side is dimmed so the kept one stands out; a huge side is capped so
/// it can't blow out the pane.
fn push_side(lines: &mut Vec<Line<'static>>, view: &SideView, text: &str, state: SideState) {
    let SideView {
        corner,
        side,
        label,
        key,
        color,
        note,
    } = *view;
    let (glyph, verdict) = state.marks();
    let dropped = state == SideState::Dropped;
    let head = if dropped {
        Style::new().fg(color).dim()
    } else {
        Style::new().fg(color).bold()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {corner} {glyph} {verdict}  "), head),
        Span::styled(format!("{side} · {label}"), head),
        Span::styled(format!("  [{key}]"), Style::new().fg(ACCENT).dim()),
        Span::styled(format!("  {note}"), Style::new().fg(color).dim()),
    ]));

    let body_style = if dropped {
        Style::new().fg(BORDER).dim()
    } else {
        Style::new().fg(color)
    };
    let body: Vec<&str> = text.lines().collect();
    // Each side is numbered from 1 rather than from its position in the file,
    // so the two sides sit line-for-line against each other: "line 3 here says
    // this, line 3 there says that" is the comparison being made, and file
    // offsets would put the same logical line on two different numbers.
    let number_width = body.len().max(1).to_string().len();
    let blank_gutter = " ".repeat(number_width);
    let gutter = |n: &str| {
        Span::styled(
            format!("  {n:>number_width$} │ "),
            Style::new().fg(color).dim(),
        )
    };
    if body.is_empty() {
        lines.push(Line::from(vec![
            gutter(&blank_gutter),
            Span::styled("(this side is empty)", Style::new().fg(color).dim()),
        ]));
        return;
    }
    const MAX: usize = 200;
    for (i, l) in body.iter().take(MAX).enumerate() {
        lines.push(Line::from(vec![
            gutter(&(i + 1).to_string()),
            Span::styled((*l).to_string(), body_style),
        ]));
    }
    if body.len() > MAX {
        lines.push(Line::from(vec![
            gutter(&blank_gutter),
            Span::styled(
                format!("… {} more line(s)", body.len() - MAX),
                Style::new().fg(color).dim(),
            ),
        ]));
    }
}

/// Up to a few lines of plain context between hunks, so the resolver reads in
/// place without dumping an entire unconflicted file into the pane.
fn context_lines(text: &str) -> Vec<String> {
    let all: Vec<&str> = text.lines().collect();
    const MAX: usize = 4;
    if all.len() <= MAX {
        return all.into_iter().map(str::to_string).collect();
    }
    let mut out: Vec<String> = all.iter().take(2).map(|s| (*s).to_string()).collect();
    out.push(format!("⋯ {} line(s)", all.len() - 3));
    out.push(all[all.len() - 1].to_string());
    out
}

/// The conflict resolver: conflicted files on the left with a resolved marker,
/// and the selected file's hunks on the right as OURS vs THEIRS blocks with the
/// current hunk and its chosen action highlighted.
#[allow(clippy::too_many_arguments)]
/// Human phrase for where the incoming ("theirs") side comes from, so the
/// resolver can say "incoming from the merge" instead of the bare "THEIRS".
fn incoming_source(kind: &ResolveKind) -> &'static str {
    match kind {
        ResolveKind::Merge => "the merge",
        // During a rebase the incoming side is your own commit being replayed,
        // not someone else's work; see `sides_are_swapped`.
        ResolveKind::Rebase => "the commit being replayed",
        ResolveKind::CherryPick => "the cherry-pick",
        // A revert's incoming side is the inverse of the commit being undone.
        ResolveKind::Revert => "the commit being undone",
        ResolveKind::StashPop { .. } => "the stash",
    }
}

/// Narrowest detail pane the three-column THEIRS → FINAL ← OURS view fits in.
/// Below this the resolver falls back to stacked side blocks, which read fine
/// in a 40-column pane where three columns would be shredded.
const RESOLVER_COLUMNS_MIN_WIDTH: usize = 76;

/// Fits one line of code into exactly `width` columns for a three-column cell:
/// tabs expanded so the columns line up, an ellipsis where a long line is cut,
/// and padded out so the next column's separator lands in the same place on
/// every row.
fn cell_text(text: &str, width: usize) -> String {
    let expanded = text.replace('\t', "    ");
    let len = expanded.chars().count();
    let mut out: String = if len > width {
        let mut s: String = expanded.chars().take(width.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        expanded
    };
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

/// The lines FINAL holds for a hunk, each tagged with the side it came from so
/// a mixed resolution shows at a glance which lines are whose.
fn final_lines(
    ours: &str,
    theirs: &str,
    action: Option<&ResolutionAction>,
) -> Vec<(String, Color)> {
    let ours_lines = || {
        ours.lines()
            .map(|l| (l.to_string(), OURS_COLOR))
            .collect::<Vec<_>>()
    };
    let theirs_lines = || {
        theirs
            .lines()
            .map(|l| (l.to_string(), THEIRS_COLOR))
            .collect::<Vec<_>>()
    };
    match action {
        None => Vec::new(),
        Some(ResolutionAction::KeepOurs) => ours_lines(),
        Some(ResolutionAction::KeepTheirs) => theirs_lines(),
        Some(ResolutionAction::KeepBoth) => {
            let mut v = ours_lines();
            v.extend(theirs_lines());
            v
        }
        Some(ResolutionAction::KeepBothReversed) => {
            let mut v = theirs_lines();
            v.extend(ours_lines());
            v
        }
    }
}

/// One hunk drawn as three columns: the incoming side on the left, what will be
/// written in the middle, and the current side on the right, with arrows
/// pointing at the middle. This is the shape every other merge tool uses, and
/// it makes "both" read as what it is — two blocks stacked in FINAL — instead
/// of a word on a header.
struct HunkColumns<'a> {
    ours: &'a str,
    theirs: &'a str,
    ours_label: &'a str,
    theirs_label: &'a str,
    action: Option<&'a ResolutionAction>,
    /// Width of one column, in cells.
    width: usize,
}

/// Renders `h` into `lines`, three cells to a row.
fn push_hunk_columns(lines: &mut Vec<Line<'static>>, h: &HunkColumns) {
    let w = h.width;
    let (ours_state, theirs_state) = side_states(h.action);
    let sep = || Span::styled(" │ ", Style::new().fg(BORDER));
    let head = |state: SideState, color: Color, text: String| {
        let (glyph, _) = state.marks();
        let style = if state == SideState::Dropped {
            Style::new().fg(color).dim()
        } else {
            Style::new().fg(color).bold()
        };
        Span::styled(cell_text(&format!("{glyph} {text}"), w), style)
    };
    lines.push(Line::from(vec![
        head(
            theirs_state,
            THEIRS_COLOR,
            format!("[t] THEIRS · {} ▶", h.theirs_label),
        ),
        sep(),
        Span::styled(cell_text("FINAL", w), Style::new().fg(theme::RESULT).bold()),
        sep(),
        head(
            ours_state,
            OURS_COLOR,
            format!("◀ OURS · {} [o]", h.ours_label),
        ),
    ]));

    let theirs: Vec<&str> = h.theirs.lines().collect();
    let ours: Vec<&str> = h.ours.lines().collect();
    let result = final_lines(h.ours, h.theirs, h.action);
    // An undecided hunk says so in FINAL rather than leaving the column blank:
    // the empty middle is exactly what the user has to act on.
    let placeholder = [("⟨ nothing chosen — t / o / b ⟩".to_string(), theme::WARNING)];
    let result: &[(String, Color)] = if h.action.is_none() {
        &placeholder
    } else if result.is_empty() {
        &[]
    } else {
        &result
    };

    const MAX: usize = 200;
    let rows = theirs.len().max(ours.len()).max(result.len()).min(MAX);
    let side_style = |state: SideState, color: Color| {
        if state == SideState::Dropped {
            Style::new().fg(BORDER).dim()
        } else {
            Style::new().fg(color)
        }
    };
    for i in 0..rows {
        let their_cell = theirs.get(i).copied().unwrap_or("");
        let our_cell = ours.get(i).copied().unwrap_or("");
        let (final_cell, final_color) = match result.get(i) {
            Some((text, color)) => (text.as_str(), *color),
            None => ("", theme::RESULT),
        };
        lines.push(Line::from(vec![
            Span::styled(
                cell_text(their_cell, w),
                side_style(theirs_state, THEIRS_COLOR),
            ),
            sep(),
            Span::styled(cell_text(final_cell, w), Style::new().fg(final_color)),
            sep(),
            Span::styled(cell_text(our_cell, w), side_style(ours_state, OURS_COLOR)),
        ]));
    }
    let hidden = theirs
        .len()
        .max(ours.len())
        .max(result.len())
        .saturating_sub(rows);
    if hidden > 0 {
        lines.push(Line::styled(
            format!("… {hidden} more line(s) — e edits this hunk in full"),
            Style::new().fg(BORDER).dim(),
        ));
    }
}

/// What one `draw_conflict_resolver` pass worked out about the pane, handed
/// back so the app can clamp keys and route clicks against what was drawn.
struct ResolverDraw {
    /// Clickable geometry of the conflicted-file list.
    list: Option<RowList>,
    /// Clickable geometry of the hunk pane.
    hits: Option<ResolverHits>,
    /// The scroll offset actually used (the hunk-following one, when the
    /// cursor has just moved).
    scroll: u16,
    /// Largest useful scroll offset for this content.
    max_scroll: u16,
    /// Height of the scrolling body, for page-sized scrolling.
    page: u16,
}

#[allow(clippy::too_many_arguments)]
fn draw_conflict_resolver(
    frame: &mut Frame,
    area: Rect,
    target: &str,
    source_label: &str,
    kind: &ResolveKind,
    files: &[String],
    resolved: &[bool],
    file: usize,
    current: Option<&ResolverFile>,
    // Path whose contents are still being read, when one is. The pane can't
    // show hunks yet and must not claim the file is resolved, so both panels
    // say it is loading instead.
    loading: Option<&str>,
    scroll: u16,
    follow: bool,
    // Whether the conflicted-file list owns the keyboard, so its chrome lights
    // up the same way the changed-file and commit panels do when focused.
    focused: bool,
) -> ResolverDraw {
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);

    // Left: conflicted files, each with a resolved/unresolved marker.
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let done = resolved.get(i).copied().unwrap_or(false);
            // The file being read gets its own marker, so pressing → says the
            // keypress landed even before the hunks are there to show.
            let mark = if loading == Some(path.as_str()) {
                Span::styled("⟳ ", Style::new().fg(ACCENT).bold())
            } else if done {
                Span::styled("✓ ", Style::new().fg(theme::SUCCESS))
            } else {
                Span::styled("• ", Style::new().fg(theme::WARNING))
            };
            let name = if done {
                Style::new().dim()
            } else {
                Style::new()
            };
            ListItem::new(Line::from(vec![mark, Span::styled(path.clone(), name)]))
        })
        .collect();
    let block = focus_panel(format!("conflicts · {target}"), focused);
    let inner = block.inner(list_area);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state =
        ListState::default().with_selected(Some(file.min(files.len().saturating_sub(1))));
    frame.render_stateful_widget(list, list_area, &mut state);
    let list_hit = (!files.is_empty()).then_some(RowList {
        inner,
        header: 0,
        offset: state.offset(),
        len: files.len(),
    });

    let path = files.get(file).map(String::as_str).unwrap_or("");

    // Right: a resolved note, or the file's hunks. Both are drawn inside one
    // panel whose top rows (the side legend) and bottom row (the key hints)
    // stay put while the hunks scroll, so the reminder of which side is which
    // is on screen at every hunk instead of scrolling away with the first one.
    let block = panel(format!("resolve · {path}"));
    let inner = block.inner(detail_area);
    frame.render_widget(block, detail_area);

    // A file still being read has no hunks yet, and it is emphatically not
    // resolved: saying so for the moment the read takes would be a lie the user
    // might act on.
    if let Some(path) = loading {
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                format!("  ⟳ loading {path}…"),
                Style::new().fg(ACCENT).bold(),
            ),
        ]);
        frame.render_widget(para, inner);
        return ResolverDraw {
            list: list_hit,
            hits: None,
            scroll: 0,
            max_scroll: 0,
            page: inner.height.max(1),
        };
    }

    let Some(rf) = current else {
        // No markers left on disk, but git only counts the file resolved once
        // it is staged. A file fixed in another editor sits in that gap, and
        // the pane has to say which state it is in rather than call both
        // "resolved".
        let staged = resolved.get(file).copied().unwrap_or(false);
        let (status, next) = if staged {
            (
                "  ✓ resolved — no conflicts remain in this file",
                "  press c to complete once every file is done",
            )
        } else {
            (
                "  ✓ no conflict markers remain in this file",
                "  press a to stage it as resolved (or r to re-read it)",
            )
        };
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::styled(status, Style::new().fg(theme::SUCCESS)),
            Line::from(""),
            Line::styled(
                format!("  incoming from {} · {source_label}", incoming_source(kind)),
                Style::new().dim(),
            ),
            Line::styled(next, Style::new().dim()),
        ]);
        frame.render_widget(para, inner);
        return ResolverDraw {
            list: list_hit,
            hits: None,
            scroll: 0,
            max_scroll: 0,
            page: inner.height.max(1),
        };
    };

    // Spell out which side is which. For a merge, OURS is what is already in
    // this worktree and THEIRS is what is being pulled in. A rebase reverses
    // that: it replays your commits on top of the other branch, so git's "ours"
    // is the branch being rebased onto and "theirs" is your own commit. Saying
    // "current" for a rebase would send the user the wrong way on every hunk.
    let swapped = kind.sides_are_swapped();
    let ours_origin = if swapped {
        "the branch you're rebasing onto"
    } else {
        "already in this worktree"
    };
    let theirs_origin = format!("incoming from {}", incoming_source(kind));

    // The legend is drawn outside the scrolling hunk list, so what each side
    // means is on screen at the last hunk as much as at the first. It used to
    // ride along at the top of the scrolled text and vanish after the first
    // hunk, which left every later hunk as two anonymous blocks of code.
    // Listed in the order the hunks put them on screen — theirs, then ours —
    // so the legend and the columns under it never disagree about which side
    // is on which hand.
    let mut legend: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("THEIRS · ", Style::new().fg(THEIRS_COLOR).bold()),
            Span::styled(
                rf.file.theirs_label.clone(),
                Style::new().fg(THEIRS_COLOR).bold(),
            ),
            Span::styled("  [t]  ", Style::new().fg(ACCENT).dim()),
            Span::styled(theirs_origin.clone(), Style::new().fg(THEIRS_COLOR)),
        ]),
        Line::from(vec![
            Span::styled("OURS · ", Style::new().fg(OURS_COLOR).bold()),
            Span::styled(
                rf.file.ours_label.clone(),
                Style::new().fg(OURS_COLOR).bold(),
            ),
            Span::styled("  [o]  ", Style::new().fg(ACCENT).dim()),
            Span::styled(ours_origin, Style::new().fg(OURS_COLOR)),
        ]),
    ];
    if swapped {
        legend.push(Line::styled(
            "! a rebase swaps the sides — \"THEIRS\" is your own work",
            Style::new().fg(theme::WARNING).bold(),
        ));
    }

    let total_hunks = rf.actions.len();
    let decided = rf.actions.iter().filter(|a| a.is_some()).count();
    // Picking a side records the choice; it does not write anything. Nothing on
    // screen used to say so, so a hunk marked "keeping BOTH" looked finished
    // while git still saw the path as unmerged and refused to complete. The
    // warning rides in the legend, which never scrolls away.
    if decided > 0 {
        legend.push(Line::from(vec![
            Span::styled(
                format!("! {decided} of {total_hunks} choice(s) not written yet — "),
                Style::new().fg(theme::WARNING).bold(),
            ),
            Span::styled("w", Style::new().fg(ACCENT).bold()),
            Span::styled(
                " saves this file (and stages it once every hunk is decided)",
                Style::new().fg(theme::WARNING),
            ),
        ]));
    }
    let legend_h = legend.len() as u16;
    // Text width inside the pane, leaving the scrollbar column free.
    let body_w = inner.width.saturating_sub(1) as usize;

    // Three columns need room for three readable cells; a narrow pane keeps the
    // stacked layout instead of shredding every line of code into ten columns.
    let columns = body_w >= RESOLVER_COLUMNS_MIN_WIDTH;
    let col_w = body_w.saturating_sub(6) / 3;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hunk_i = 0usize;
    // Line offset of the current hunk's header, used to keep it in view.
    let mut current_line = 0usize;
    // First and last line of each hunk's block, so a click can find the hunk
    // (and the wheel can be told what it is scrolling over).
    let mut hunk_spans: Vec<(usize, usize)> = Vec::new();
    for seg in &rf.file.segments {
        match seg {
            ConflictSegment::Plain(text) => {
                for l in context_lines(text) {
                    lines.push(Line::styled(format!("    {l}"), Style::new().dim()));
                }
            }
            ConflictSegment::Hunk { ours, theirs, .. } => {
                let is_cur = hunk_i == rf.hunk;
                let block_start = lines.len();
                if is_cur {
                    current_line = lines.len();
                }
                let action = rf.actions.get(hunk_i).and_then(|a| a.as_ref());
                let (label, color) = action_label(action);
                let (ours_state, theirs_state) = side_states(action);
                let marker = if is_cur { "◆" } else { "◇" };
                let hstyle = if is_cur {
                    Style::new().bg(SELECTION_BG).bold()
                } else {
                    Style::new().bold()
                };
                let head = format!("{marker} hunk {} of {total_hunks} · ", hunk_i + 1);
                // Pad the current hunk's header out to the pane width so its
                // highlight reads as a full selected row, not a stray patch of
                // background behind the text.
                let pad = body_w.saturating_sub(head.chars().count() + label.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(head, hstyle.fg(ACCENT)),
                    Span::styled(label, hstyle.fg(color)),
                    Span::styled(" ".repeat(if is_cur { pad } else { 0 }), hstyle),
                ]));
                if columns {
                    push_hunk_columns(
                        &mut lines,
                        &HunkColumns {
                            ours,
                            theirs,
                            ours_label: &rf.file.ours_label,
                            theirs_label: &rf.file.theirs_label,
                            action,
                            width: col_w,
                        },
                    );
                } else {
                    push_side(
                        &mut lines,
                        &SideView {
                            corner: "┌",
                            side: "OURS",
                            label: &rf.file.ours_label,
                            key: 'o',
                            color: OURS_COLOR,
                            note: "",
                        },
                        ours,
                        ours_state,
                    );
                    push_side(
                        &mut lines,
                        &SideView {
                            corner: "├",
                            side: "THEIRS",
                            label: &rf.file.theirs_label,
                            key: 't',
                            color: THEIRS_COLOR,
                            note: "",
                        },
                        theirs,
                        theirs_state,
                    );
                }
                lines.push(Line::from(vec![
                    Span::styled("  └ ", Style::new().fg(BORDER)),
                    Span::styled("b", Style::new().fg(Color::Cyan).bold()),
                    Span::styled(" both · ", Style::new().dim()),
                    Span::styled("⇧B", Style::new().fg(Color::Cyan).bold()),
                    Span::styled(" both, theirs 1st · ", Style::new().dim()),
                    Span::styled("e", Style::new().fg(MANUAL_COLOR).bold()),
                    Span::styled(" edit hunk", Style::new().dim()),
                ]));
                // The trailing blank belongs to the hunk for hit-testing too,
                // so there is no dead row between one hunk and the next.
                lines.push(Line::from(""));
                hunk_spans.push((block_start, lines.len().saturating_sub(1)));
                hunk_i += 1;
            }
        }
    }

    let [legend_area, body_area, hint_area] = Layout::vertical([
        Constraint::Length(legend_h),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(legend), legend_area);

    // The pane scrolls on its own (wheel, Shift+↑/↓, PageUp/Down) and only
    // jumps to the cursor when the cursor is what moved: pinning it to the
    // current hunk on every frame is what used to make the bottom of a tall
    // hunk unreachable.
    let total = lines.len();
    let height = body_area.height as usize;
    let max_scroll = total.saturating_sub(height);
    let scroll = if follow {
        // Park the current hunk's header one line down from the top: the whole
        // hunk is then on screen when it fits, and starts at its first line
        // when it doesn't.
        let first = hunk_spans
            .get(rf.hunk)
            .map(|(first, _)| *first)
            .unwrap_or(current_line);
        first.saturating_sub(1).min(max_scroll)
    } else {
        (scroll as usize).min(max_scroll)
    } as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body_area);
    let mut sb = ScrollbarState::new(max_scroll).position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::new().fg(BORDER))
            .thumb_style(Style::new().fg(ACCENT)),
        body_area,
        &mut sb,
    );

    let hint = Line::from(vec![
        Span::styled(
            format!("{decided}/{total_hunks} decided · "),
            Style::new().fg(if decided == total_hunks {
                theme::SUCCESS
            } else {
                theme::WARNING
            }),
        ),
        Span::styled("⇧E", Style::new().fg(ACCENT).bold()),
        Span::styled(" edit file · ", Style::new().dim()),
        Span::styled("⇧O/⇧T", Style::new().fg(ACCENT).bold()),
        Span::styled(" take side · ", Style::new().dim()),
        Span::styled("w", Style::new().fg(ACCENT).bold()),
        Span::styled(" stage", Style::new().dim()),
    ]);
    frame.render_widget(Paragraph::new(hint), hint_area);
    // The manual hunk editor floats over the resolver as a modal (`draw_modal`).
    ResolverDraw {
        list: list_hit,
        hits: Some(ResolverHits {
            body: body_area,
            scroll,
            hunks: hunk_spans,
            // Cell x of the first column past THEIRS, and past FINAL. The
            // separators are three cells wide and belong to neither side.
            columns: columns.then(|| {
                (
                    body_area.x + col_w as u16 + 3,
                    body_area.x + (col_w as u16) * 2 + 6,
                )
            }),
        }),
        scroll,
        max_scroll: max_scroll as u16,
        page: body_area.height.max(1),
    }
}

/// Style for one raw line of a conflicted file in the conflict editor: the
/// conflict markers themselves stand out, and each side keeps the color it has
/// in the resolver so the file reads the same way there and here.
fn conflict_line_style(line: &str, side: &mut Color) -> Style {
    if line.starts_with("<<<<<<<") {
        *side = OURS_COLOR;
        return Style::new().fg(theme::WARNING).bold();
    }
    if line.starts_with("|||||||") {
        *side = BORDER;
        return Style::new().fg(theme::WARNING).bold();
    }
    if line.trim_end() == "=======" {
        *side = THEIRS_COLOR;
        return Style::new().fg(theme::WARNING).bold();
    }
    if line.starts_with(">>>>>>>") {
        let style = Style::new().fg(theme::WARNING).bold();
        *side = Color::Reset;
        return style;
    }
    Style::new().fg(*side)
}

/// The conflict editor: the conflicted file exactly as it sits on disk, with
/// line numbers, the conflict markers highlighted, and a visible block cursor.
/// Saved with Ctrl+S, discarded with Esc.
fn draw_file_editor(frame: &mut Frame, area: Rect, path: &str, editor: &super::app::TextArea) {
    // The whole frame: this is a real source file, and a centred box with a
    // margin buys nothing but fewer lines and columns of the thing being read.
    frame.render_widget(Clear, area);
    let block = dialog_panel(format!(
        "edit {path} · Ctrl+S save · Esc cancel · ←/→/↑/↓ PgUp/PgDn move"
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let height = inner.height as usize;
    // No scroll offsets are stored, so both are derived from the cursor: keep
    // it centered vertically and just inside the edge horizontally, clamped at
    // the ends of the file and of the line.
    let max_scroll = editor.lines.len().saturating_sub(height);
    let scroll = editor.row.saturating_sub(height / 2).min(max_scroll);
    let gutter_width = editor.lines.len().to_string().len();
    // What is left for the text itself once the line-number gutter is drawn.
    let text_width = (inner.width as usize).saturating_sub(gutter_width + 1);
    // Long lines used to be silently clipped at the right edge with no way to
    // see past them; follow the cursor instead, keeping a few columns of lead-in
    // so the character being typed is never flush against the border.
    let h_scroll = editor.col.saturating_sub(text_width.saturating_sub(4));

    // The side each line belongs to is carried down from the last marker seen,
    // so it has to be tracked from the top of the file, not from the first
    // visible row.
    let mut side = Color::Reset;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (r, text) in editor.lines.iter().enumerate() {
        let style = conflict_line_style(text, &mut side);
        if r < scroll || r >= scroll + height {
            continue;
        }
        let gutter = Span::styled(
            format!("{:>width$} ", r + 1, width = gutter_width),
            Style::new().fg(BORDER),
        );
        // Everything below indexes the horizontally scrolled slice of the line,
        // so the cursor column is rebased into it too.
        let chars: Vec<char> = text.chars().skip(h_scroll).collect();
        if r == editor.row {
            // Split the cursor line so the character under the cursor is shown
            // inverted, giving a visible caret (or a trailing block at line end).
            let col = editor.col.saturating_sub(h_scroll).min(chars.len());
            let cursor_style = Style::new().bg(ACCENT).fg(Color::Black);
            let mut spans = vec![
                gutter,
                Span::styled(chars[..col].iter().collect::<String>(), style),
            ];
            if col < chars.len() {
                spans.push(Span::styled(chars[col].to_string(), cursor_style));
                spans.push(Span::styled(
                    chars[col + 1..].iter().collect::<String>(),
                    style,
                ));
            } else {
                spans.push(Span::styled(" ", cursor_style));
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                gutter,
                Span::styled(chars.into_iter().collect::<String>(), style),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A small centered overlay showing that a background op is running.
fn draw_busy(frame: &mut Frame, area: Rect, label: &str, tick: u64) {
    let text = format!("{} {label}", spinner_glyph(tick));
    let popup = centered(area, (text.chars().count() as u16 + 6).min(area.width), 3);
    frame.render_widget(Clear, popup);
    let para = Paragraph::new(Line::styled(text, Style::new().fg(ACCENT).bold()))
        .block(dialog_panel("please wait"));
    frame.render_widget(para, popup);
}

/// Braille throbber frame for the current tick; the event loop redraws often
/// enough (~10fps) that cycling by `tick` reads as a smooth spinner.
fn spinner_glyph(tick: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick % FRAMES.len() as u64) as usize]
}

/// Renders whichever `Modal` is active over the current view: a confirmation
/// (body text plus selectable option rows), a single-line prompt, or the manual
/// hunk editor. Replaces the former per-dialog `draw_confirm_*`/`draw_input_popup`
/// functions.
fn draw_modal(frame: &mut Frame, area: Rect, app: &App) -> Option<RowList> {
    let Some(modal) = &app.modal else {
        return None;
    };
    match modal {
        Modal::Confirm {
            title,
            body,
            options,
            selected,
            ..
        } => {
            let mut lines = body.clone();
            if !options.is_empty() {
                lines.push(Line::from(""));
            }
            let header_lines = lines.len() as u16;
            for (i, opt) in options.iter().enumerate() {
                lines.push(modal_option(i == *selected, opt));
            }
            let width = modal_width(&lines);
            let popup = modal_rect(area, lines.len() as u16, width, 2);
            frame.render_widget(Clear, popup);
            let block = dialog_panel(title.clone());
            let inner = block.inner(popup);
            frame.render_widget(Paragraph::new(lines).block(block), popup);
            (!options.is_empty()).then_some(RowList {
                inner: Rect {
                    x: inner.x,
                    y: inner.y + header_lines,
                    width: inner.width,
                    height: options.len() as u16,
                },
                header: 0,
                offset: 0,
                len: options.len(),
            })
        }
        Modal::Prompt {
            title, input, hint, ..
        } => {
            let popup = modal_rect(area, 2, 64, 2);
            frame.render_widget(Clear, popup);
            let lines = vec![
                prompt_line_at(input.as_str(), input.cursor),
                Line::from(hint.clone().dim()),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(dialog_panel(title.clone())),
                popup,
            );
            None
        }
        Modal::FileEditor { path, editor } => {
            draw_file_editor(frame, area, path, editor);
            None
        }
    }
}

/// A confirm-modal option row: a radio marker plus the label, highlighted when
/// selected and dimmed when disabled (an option shown but not choosable).
fn modal_option(selected: bool, opt: &ConfirmOption) -> Line<'static> {
    let marker = if selected { "▌ ● " } else { "  ○ " };
    let base = if opt.enabled {
        Style::new()
    } else {
        Style::new().dim()
    };
    let style = if selected {
        base.bg(SELECTION_BG).bold()
    } else {
        base
    };
    let mut spans = vec![
        Span::styled(marker.to_string(), style.fg(ACCENT)),
        Span::styled(opt.label.clone(), style),
    ];
    // Show the direct-select shortcut (destructive options are the Shift-variant)
    // so a force/delete key is discoverable without opening help.
    if let Some(c) = opt.shortcut() {
        spans.push(Span::styled(format!("  ({c})"), style.fg(ACCENT)));
    }
    Line::from(spans)
}

/// Width for a confirm modal: wide enough for its widest line plus the panel
/// chrome, clamped to a sensible range.
fn modal_width(lines: &[Line]) -> u16 {
    let content = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    content.saturating_add(6).clamp(48, 80)
}

/// Footer key hints for the active modal.
fn modal_footer_hints(modal: &Modal) -> &'static [Binding] {
    const CONFIRM_MULTI: &[Binding] = &[
        hint("↑/↓", "choose"),
        hint("Enter", "confirm"),
        hint("Esc", "cancel"),
    ];
    const CONFIRM_SINGLE: &[Binding] = &[hint("Enter", "confirm"), hint("Esc", "cancel")];
    const PROMPT: &[Binding] = &[
        hint("type", "then"),
        hint("Enter", "confirm"),
        hint("Esc", "cancel"),
    ];
    const FILE: &[Binding] = &[
        hint("type", "edit the file"),
        hint("↑↓←→/wheel", "move"),
        hint("Ctrl+S", "save to disk"),
        hint("Esc", "cancel"),
    ];
    match modal {
        Modal::Confirm { options, .. } if options.len() > 1 => CONFIRM_MULTI,
        Modal::Confirm { .. } => CONFIRM_SINGLE,
        Modal::Prompt { .. } => PROMPT,
        Modal::FileEditor { .. } => FILE,
    }
}

/// A centered popup rect sized to fit `content_height` rows of actual
/// content plus `chrome` extra rows (borders, a tab bar, a hint line —
/// whatever the caller draws around the content), clamped so a very long
/// modal still leaves a margin around the frame instead of filling it, and
/// never smaller than `chrome` plus one row of content.
///
/// Shared by `draw_help` and `draw_error_popup`, whose popups both size to
/// their content instead of a fixed literal.
fn modal_rect(area: Rect, content_height: u16, width: u16, chrome: u16) -> Rect {
    let max_height = (area.height * 9 / 10).max(chrome + 1);
    let height = content_height
        .saturating_add(chrome)
        .max(chrome + 1)
        .min(max_height);
    centered(area, width, height)
}

/// A rect of `width` x `height` centered inside `area`, clamped to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::super::config_editor::{
        BRANCHES_REFRESH_ROW, COPY_ROW, LAYOUT_ROW, OPEN_COMMAND_ROW, RUN_ROW, StringListEditor,
        StringListKind, THEME_ROW, UPDATE_ROW, check_line, line_of_row, preview_line,
        theme_preview_label_line, theme_preview_line, version_line,
    };
    use super::*;
    use crate::git::LogEntry;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders `draw` into an off-screen terminal and returns what each row of
    /// the buffer reads as, so a test can assert on the drawn output.
    fn render(width: u16, height: u16, draw: impl FnOnce(&mut Frame, Rect)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The gutter puts each line's own number beside it, sized to the widest,
    /// and leaves headers and hunk markers blank.
    #[test]
    fn diff_gutter_numbers_each_line_from_the_hunk_header() {
        let diff = "@@ -8,2 +98,3 @@\n ctx\n-gone\n+added\n";
        let lines = diff_lines_with_gutter("x.rs", diff, true);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans[0].content.to_string())
            .collect();
        assert_eq!(
            text,
            ["   ", "98 ", " 9 ", "99 "],
            "hunk header blank; context/added on the new side, removed on the old"
        );
    }

    /// With the setting off the diff renders exactly as before: no gutter span
    /// is prepended at all.
    #[test]
    fn diff_gutter_is_absent_when_line_numbers_are_off() {
        let diff = "@@ -1,1 +1,1 @@\n+added\n";
        let plain = diff_lines_with_gutter("x.rs", diff, false);
        assert_eq!(plain, highlight::diff_lines("x.rs", diff));
    }

    /// A diff with no hunk headers (binary, or a status-only summary) has no
    /// numbers to show, so it gets no empty gutter column either.
    #[test]
    fn diff_gutter_is_skipped_when_nothing_can_be_numbered() {
        let diff = "Binary files a/x.png and b/x.png differ\n";
        assert_eq!(
            diff_lines_with_gutter("x.png", diff, true),
            highlight::diff_lines("x.png", diff)
        );
    }

    fn entry(hash: &str, subject: &str, refs: &[&str]) -> LogEntry {
        LogEntry {
            hash: hash.to_string(),
            subject: subject.to_string(),
            author: "Ada".to_string(),
            date: "1 hour ago".to_string(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }
    }

    /// The tree view draws git's art as box-drawing characters, keeps the
    /// art-only connector rows, decorates the refs, and pads every row's art to
    /// the widest so the hashes line up in one column.
    #[test]
    fn log_tree_draws_graph_art_and_refs() {
        let rows = vec![
            GraphLine {
                graph: "* ".into(),
                entry: Some(entry("1a2b3c4", "merge feature", &["HEAD -> main"])),
            },
            GraphLine {
                graph: "|\\".into(),
                entry: None,
            },
            GraphLine {
                graph: "| * ".into(),
                entry: Some(entry("5d6e7f8", "add tests", &[])),
            },
        ];
        let out = render(78, 6, |frame, area| {
            draw_log(frame, area, "main", &rows, &[], 0, LogMode::Tree);
        });
        assert!(out[0].contains("log · main · tree"), "{out:#?}");
        // git's `*` and `|` become `●` and `│`; the `\` becomes `╲`.
        assert!(
            out[1].contains("●   1a2b3c4 (HEAD -> main) merge feature"),
            "{out:#?}"
        );
        assert!(out[2].contains("│╲"), "{out:#?}");
        assert!(out[3].contains("│ ● 5d6e7f8 add tests"), "{out:#?}");
        // The trunk commit's art is one column wide and the branch commit's is
        // three, so only padding to a shared width can put both hashes here.
        let hash_col = |line: &str| line.find("1a2b3c4").or_else(|| line.find("5d6e7f8"));
        assert_eq!(hash_col(&out[1]), hash_col(&out[3]), "{out:#?}");
    }

    /// The graph column is as wide as the widest art any row draws, ignoring the
    /// trailing padding git leaves on a merge line, and never wider than the cap.
    #[test]
    fn graph_width_ignores_trailing_pad_and_stops_at_the_cap() {
        let rows = |arts: &[&str]| -> Vec<GraphLine> {
            arts.iter()
                .map(|a| GraphLine {
                    graph: (*a).to_string(),
                    entry: None,
                })
                .collect()
        };
        // "*   " is a merge line: git pads it for the parents it is about to
        // fan, and counting that would indent the whole log for nothing.
        assert_eq!(graph_width(&rows(&["* ", "*   ", "| * "])), 3);
        assert_eq!(graph_width(&rows(&[])), 0);
        // Flat mode carries no art at all.
        assert_eq!(graph_width(&rows(&["", ""])), 0);
        let wide = "| ".repeat(40);
        assert_eq!(graph_width(&rows(&[&wide])), GRAPH_WIDTH_MAX);
    }

    /// A commit's hash takes the color of the lane its node sits in, so hash and
    /// dot read as one branch line. Flat rows have no lane and keep yellow.
    #[test]
    fn the_hash_takes_its_lanes_color() {
        assert_eq!(lane_color("* "), Some(GRAPH_COLORS[0]));
        assert_eq!(lane_color("| * "), Some(GRAPH_COLORS[1]));
        assert_eq!(lane_color("| | * "), Some(GRAPH_COLORS[2]));
        // A connector-only row has no node, and neither does a flat-mode row.
        assert_eq!(lane_color("|\\"), None);
        assert_eq!(lane_color(""), None);

        // And the color reaches the screen: the second commit is one lane over,
        // so its hash must not match the trunk commit's.
        let rows = vec![
            GraphLine {
                graph: "* ".into(),
                entry: Some(entry("1a2b3c4", "merge feature", &[])),
            },
            GraphLine {
                graph: "| * ".into(),
                entry: Some(entry("5d6e7f8", "add tests", &[])),
            },
        ];
        let backend = ratatui::backend::TestBackend::new(78, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_log(frame, frame.area(), "main", &rows, &[], 0, LogMode::Tree);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let hash_fg = |line: &str, y: u16| {
            let x = line
                .find("1a2b3c4")
                .or_else(|| line.find("5d6e7f8"))
                .unwrap() as u16;
            buf[(x, y)].style().fg
        };
        let text: Vec<String> = (0..5)
            .map(|y| (0..78).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert_eq!(hash_fg(&text[1], 1), Some(GRAPH_COLORS[0]), "{text:#?}");
        assert_eq!(hash_fg(&text[2], 2), Some(GRAPH_COLORS[1]), "{text:#?}");
    }

    /// A file still being read is neither shown as hunks nor claimed resolved:
    /// both panels say it is loading, so pressing → reads as registered.
    #[test]
    fn the_resolver_says_which_file_it_is_loading() {
        let out = render(78, 8, |frame, area| {
            draw_conflict_resolver(
                frame,
                area,
                "wt",
                "feature/login",
                &ResolveKind::Merge,
                &["src/main.rs".to_string(), "src/lib.rs".to_string()],
                &[false, false],
                1,
                None,
                Some("src/lib.rs"),
                0,
                false,
                true,
            );
        });
        let screen = out.join("\n");
        assert!(screen.contains("loading src/lib.rs…"), "{out:#?}");
        assert!(
            !screen.contains("resolved — no conflicts"),
            "a loading file must not be called resolved: {out:#?}"
        );
        // The row being read is marked in the file list too.
        assert!(out.iter().any(|l| l.contains("⟳ src/lib.rs")), "{out:#?}");
    }

    /// The footer and the help panel now read the same bindings, so a help-only
    /// entry must not leak into the footer and the hints must stay exactly what
    /// they were before the two lists were merged.
    #[test]
    fn footer_hints_skip_help_only_bindings() {
        let line = hint_line_fitting(help::WORKTREES, None).to_string();
        assert_eq!(
            line,
            "⇥ tabs  Enter changes  n new  b switch branch  c commit  o open  \
             s stash  p pull  ⇧P push  l log  d delete  x resolve  ? help  q quit"
        );
        // `u`, `e`, `m`, `f`, `⇧R` and the cursor keys are documented in help
        // but have no footer label, so they are absent above.
        assert!(!line.contains("select worktree"), "{line}");
        assert!(!line.contains("move changes"), "{line}");
    }

    #[test]
    fn footer_hints_truncate_to_width() {
        let line = hint_line_fitting(help::WORKTREES, Some(40)).to_string();
        assert!(line.contains('…'), "expected ellipsis, got {line}");
        assert!(line.chars().count() <= 40, "overflow: {line}");
        // Left-to-right fill keeps the early, high-traffic keys.
        assert!(line.starts_with("⇥ tabs"), "{line}");
    }

    /// Every help tab is reachable from the bar, and the active one is marked.
    #[test]
    fn help_tab_bar_draws_every_tab() {
        let out = render(78, 1, |frame, area| {
            draw_help_tabs(frame, area, HelpTab::Changes)
        });
        for tab in HelpTab::ALL {
            assert!(
                out[0].contains(tab.title()),
                "{} missing: {out:#?}",
                tab.title()
            );
        }
        // The six titles have to fit the panel's width without being clipped.
        assert!(!out[0].ends_with('…'), "{out:#?}");
    }

    /// The flat view is the same rows with no art and no blank connector lines.
    #[test]
    fn log_flat_draws_commits_without_art() {
        let rows = vec![GraphLine {
            graph: String::new(),
            entry: Some(entry("1a2b3c4", "fix parser", &[])),
        }];
        let out = render(78, 4, |frame, area| {
            draw_log(frame, area, "main", &rows, &[], 0, LogMode::Flat);
        });
        assert!(out[0].contains("log · main · flat"), "{out:#?}");
        assert!(out[1].contains("1a2b3c4 fix parser"), "{out:#?}");
        assert!(!out[1].contains('●'), "{out:#?}");
    }

    /// Branch commits keep their checkbox column, and art-only rows indent past
    /// it so the graph still lines up.
    #[test]
    fn branch_commits_align_art_rows_under_the_checkbox() {
        let rows = vec![
            GraphLine {
                graph: "* ".into(),
                entry: Some(entry("1a2b3c4d5e", "merge feature", &[])),
            },
            GraphLine {
                graph: "|\\".into(),
                entry: None,
            },
        ];
        let out = render(78, 5, |frame, area| {
            draw_branch_commits(
                frame,
                area,
                "main",
                &rows,
                &[true, false],
                &[],
                0,
                LogMode::Tree,
            );
        });
        assert!(out[0].contains("commits · main · tree"), "{out:#?}");
        // A marked commit, its art, then the hash abbreviated to 9 chars.
        assert!(
            out[1].contains("[x] ●  1a2b3c4d5 merge feature"),
            "{out:#?}"
        );
        // The connector must sit under the commit's lane rather than under the
        // checkbox column. Both searches skip the panel's left border, which is
        // itself a `│`.
        let column = |row: &str, needle: char| {
            row.chars()
                .skip(1)
                .position(|c| c == needle)
                .map(|i| i + 1)
                .unwrap_or_else(|| panic!("no {needle} in {row:?}"))
        };
        assert_eq!(
            column(&out[2], '│'),
            column(&out[1], '●'),
            "art row misaligned: {out:#?}"
        );
    }

    #[test]
    fn empty_log_says_so() {
        let out = render(40, 4, |frame, area| {
            draw_log(frame, area, "main", &[], &[], 0, LogMode::Tree);
        });
        assert!(out[1].contains("no commits"), "{out:#?}");
    }

    /// An editor with a known worktree_dir, for the settings render tests.
    fn settings_editor(auto_update_check: &str) -> ConfigEditor {
        ConfigEditor {
            repo_root: std::path::PathBuf::from("/tmp/proj"),
            global_config: None,
            fields: crate::settings::RepoConfigFields {
                worktree_dir: "inside".to_string(),
                auto_update_check: auto_update_check.to_string(),
                ..Default::default()
            },
            selected: 0,
            editing: None,
            open_list: None,
            string_list: None,
        }
    }

    /// The line offsets `config_editor::row_at_line` decodes clicks with must
    /// match what is actually drawn, or clicking a row would select another.
    #[test]
    fn settings_tab_draws_rows_where_the_click_decoder_expects_them() {
        let editor = settings_editor("false");
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        // The form starts after the panel border and one blank spacer line.
        let form_start = 2;
        let line = |offset: usize| out[form_start + offset].as_str();

        for (row, label) in [
            (0, "worktree_dir"),
            (1, "open_command"),
            (2, "setup.copy"),
            (3, "setup.run"),
            (UPDATE_ROW, "auto_update_check"),
            (THEME_ROW, "diff_theme"),
            (LAYOUT_ROW, "worktrees_layout"),
            (BRANCHES_REFRESH_ROW, "branches_refresh_mins"),
        ] {
            let offset = line_of_row(row);
            assert!(
                line(offset).contains(label),
                "row {row} should be {label}: {:?}",
                line(offset)
            );
        }
        assert!(
            line(line_of_row(UPDATE_ROW)).contains("off"),
            "{:?}",
            line(line_of_row(UPDATE_ROW))
        );
        assert!(
            line(line_of_row(THEME_ROW)).contains("default"),
            "{:?}",
            line(line_of_row(THEME_ROW))
        );
        assert!(
            line(line_of_row(LAYOUT_ROW)).contains("default: two panels"),
            "{:?}",
            line(line_of_row(LAYOUT_ROW))
        );
        assert!(
            line(line_of_row(BRANCHES_REFRESH_ROW)).contains("default: 10"),
            "{:?}",
            line(line_of_row(BRANCHES_REFRESH_ROW))
        );
        // Theme preview belongs under the theme row, before layout.
        assert!(
            theme_preview_label_line() > line_of_row(THEME_ROW),
            "theme preview must follow the theme row"
        );
        assert!(
            theme_preview_label_line() < line_of_row(LAYOUT_ROW),
            "theme preview must sit above the layout row"
        );
        assert!(
            line(theme_preview_label_line()).contains("diff colours look like"),
            "{:?}",
            line(theme_preview_label_line())
        );
        assert!(
            line(0).contains("This repo"),
            "repo section header missing: {:?}",
            line(0)
        );
        assert!(
            line(line_of_row(UPDATE_ROW) - 2).contains("All repos"),
            "global section header missing: {:?}",
            line(line_of_row(UPDATE_ROW) - 2)
        );
        assert!(
            line(line_of_row(0) + 1).contains("Where new worktrees go"),
            "description missing under worktree_dir: {:?}",
            line(line_of_row(0) + 1)
        );
        assert!(
            line(theme_preview_label_line()).contains("Eighties"),
            "default theme label should appear: {:?}",
            line(theme_preview_label_line())
        );
        assert!(
            line(theme_preview_line()).contains("@@"),
            "sample hunk header missing: {:?}",
            line(theme_preview_line())
        );
        assert!(
            line(theme_preview_line() + 1).contains("fn greet"),
            "sample removal missing: {:?}",
            line(theme_preview_line() + 1)
        );
        assert!(
            line(theme_preview_line() + 2).contains("fn greet"),
            "sample addition missing: {:?}",
            line(theme_preview_line() + 2)
        );
        assert!(line(preview_line()).contains("new worktrees go in"));
        assert!(line(version_line()).contains(CURRENT_VERSION));
        assert!(line(check_line()).contains("check for updates"));
    }

    #[test]
    fn settings_tab_theme_preview_follows_the_selected_theme() {
        let mut editor = settings_editor("");
        editor.fields.diff_theme = "ocean".to_string();
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let label = &out[2 + theme_preview_label_line()];
        assert!(label.contains("Ocean"), "{label}");
        assert!(
            out[2 + theme_preview_line() + 1].contains("fn greet"),
            "{out:#?}"
        );
    }

    #[test]
    fn settings_tab_shows_the_version_and_any_update() {
        let editor = settings_editor("");
        // With nothing newer found, the version line says so.
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let ver = &out[2 + version_line()];
        assert!(ver.contains(CURRENT_VERSION), "{ver}");
        assert!(ver.contains("up to date"), "{ver}");
        // The unset toggle spells out which default it inherits.
        assert!(
            out[2 + line_of_row(UPDATE_ROW)].contains("default"),
            "{out:#?}"
        );

        // A found release is called out instead.
        let release = Release {
            tag: "v9.9.9".to_string(),
            version: "9.9.9".to_string(),
            url: String::new(),
        };
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, Some(&release));
        });
        let ver = &out[2 + version_line()];
        assert!(ver.contains("9.9.9 available"), "{ver}");
    }

    /// The list editor draws every command, the add row, and the done row, and
    /// covers the form (which reports no clickable rows while it is up).
    #[test]
    fn settings_tab_draws_the_open_command_list_editor() {
        let mut editor = settings_editor("");
        editor.selected = OPEN_COMMAND_ROW;
        editor.fields.open_command = vec![
            OpenCommand::new("cursor {path}"),
            OpenCommand::new("open {path}"),
        ];
        let mut list = OpenCommandEditor::new(editor.fields.open_command.clone());
        list.selected = 1;
        editor.open_list = Some(list);
        let hit = std::cell::Cell::new(true);
        let out = render(90, 48, |frame, area| {
            hit.set(draw_settings_tab(frame, area, &editor, None).is_some());
        });
        let text = out.join("\n");
        assert!(text.contains("open commands"), "{text}");
        assert!(text.contains("cursor {path}"), "{text}");
        assert!(text.contains("add a command"), "{text}");
        assert!(text.contains("[ done ]"), "{text}");
        assert!(text.contains("Enter edit"), "{text}");
        assert!(!hit.get(), "the modal list must swallow clicks");
    }

    /// With no list open, the row summarises the configured commands rather
    /// than showing a single joined value.
    #[test]
    fn settings_tab_summarises_multiple_open_commands() {
        let mut editor = settings_editor("");
        editor.fields.open_command = vec![
            OpenCommand::new("cursor {path}"),
            OpenCommand::new("open {path}"),
        ];
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let row = &out[2 + line_of_row(OPEN_COMMAND_ROW)];
        assert!(row.contains("2 commands"), "{row}");
    }

    #[test]
    fn settings_tab_draws_the_setup_run_list_editor() {
        let mut editor = settings_editor("");
        editor.selected = RUN_ROW;
        editor.fields.run = vec!["npm install".to_string(), "npm run build".to_string()];
        let mut list = StringListEditor::new(StringListKind::Run, editor.fields.run.clone());
        list.selected = 1;
        editor.string_list = Some(list);
        let hit = std::cell::Cell::new(true);
        let out = render(90, 48, |frame, area| {
            hit.set(draw_settings_tab(frame, area, &editor, None).is_some());
        });
        let text = out.join("\n");
        assert!(text.contains("setup commands"), "{text}");
        assert!(text.contains("npm install"), "{text}");
        assert!(text.contains("add a command"), "{text}");
        assert!(text.contains("[ done ]"), "{text}");
        assert!(text.contains("Enter edit"), "{text}");
        assert!(!text.contains("g global"), "{text}");
        assert!(!hit.get(), "the modal list must swallow clicks");
    }

    #[test]
    fn settings_tab_draws_the_setup_copy_list_editor() {
        let mut editor = settings_editor("");
        editor.selected = COPY_ROW;
        editor.fields.copy = vec![".env".to_string()];
        editor.string_list = Some(StringListEditor::new(
            StringListKind::Copy,
            editor.fields.copy.clone(),
        ));
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let text = out.join("\n");
        assert!(text.contains("copy files"), "{text}");
        assert!(text.contains(".env"), "{text}");
        assert!(text.contains("add a file"), "{text}");
    }

    #[test]
    fn settings_tab_summarises_multiple_setup_items() {
        let mut editor = settings_editor("");
        editor.fields.copy = vec![".env".to_string(), ".env.local".to_string()];
        editor.fields.run = vec!["npm ci".to_string(), "npm run build".to_string()];
        let out = render(90, 48, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let copy = &out[2 + line_of_row(COPY_ROW)];
        assert!(copy.contains("2 files"), "{copy}");
        let run = &out[2 + line_of_row(RUN_ROW)];
        assert!(run.contains("2 commands"), "{run}");
    }

    /// The picker previews what will actually run, so the rows carry the
    /// selected worktree's path rather than the raw `{path}` template.
    #[test]
    fn open_command_pick_draws_expanded_commands() {
        let out = render(90, 12, |frame, area| {
            draw_open_command_pick(
                frame,
                area,
                "feature",
                &OpenCommandVars {
                    path: "/tmp/proj-feature",
                    name: "feature",
                    branch: "feature",
                    status: "ahead",
                },
                &[
                    OpenCommand::new("cursor {path}"),
                    OpenCommand::new("open {path} # {branch}").with_mode(CommandMode::Terminal),
                ],
                0,
            );
        });
        let text = out.join("\n");
        assert!(text.contains("open 'feature' with…"), "{text}");
        assert!(text.contains("cursor /tmp/proj-feature"), "{text}");
        assert!(text.contains("open /tmp/proj-feature # feature"), "{text}");
        assert!(!text.contains("{path}"), "templates must not be shown raw");
    }

    #[test]
    fn spinner_glyph_cycles_through_all_frames() {
        // Every tick maps to a braille frame and the sequence wraps cleanly.
        let first = (0..10).map(spinner_glyph).collect::<Vec<_>>();
        assert_eq!(first.len(), 10);
        assert_eq!(spinner_glyph(0), spinner_glyph(10), "wraps after 10 frames");
        assert_eq!(spinner_glyph(3), spinner_glyph(13));
        // Guard against an out-of-range index panic at the u64 boundary.
        let _ = spinner_glyph(u64::MAX);
    }

    #[test]
    fn help_binding_continuation_hangs_under_the_description() {
        // Narrow width forces a wrap; the second line must start under the
        // description, not at column 0.
        let width = HELP_KEY_COL + 10;
        let lines = help_binding_lines(
            "Space",
            "cycle auto_update_check, diff_theme, or worktrees_layout",
            width,
        );
        assert!(lines.len() > 1, "expected wrap: {lines:?}");
        let first = lines[0].to_string();
        let second = lines[1].to_string();
        assert!(first.contains("Space"), "{first}");
        assert!(
            second.starts_with(&" ".repeat(HELP_KEY_COL)),
            "continuation must hang under the description: {second:?}"
        );
        assert!(
            !second[HELP_KEY_COL..].starts_with(' '),
            "indent should be exact, not deeper: {second:?}"
        );
    }

    /// The resolver's conflicted-file list is a focus target in the three-panel
    /// layout, so its border must light up like the changed-file and commit
    /// panels do instead of staying dim whichever panel owns the keyboard.
    #[test]
    fn resolver_file_list_lights_up_when_focused() {
        let border_fg = |focused: bool| {
            let mut terminal = Terminal::new(TestBackend::new(78, 8)).unwrap();
            terminal
                .draw(|frame| {
                    draw_conflict_resolver(
                        frame,
                        frame.area(),
                        "wt",
                        "feature/login",
                        &ResolveKind::Merge,
                        &["src/main.rs".to_string()],
                        &[false],
                        0,
                        None,
                        None,
                        0,
                        false,
                        focused,
                    );
                })
                .unwrap();
            terminal.backend().buffer()[(0, 0)].style().fg
        };
        assert_eq!(
            border_fg(true),
            Some(ACCENT),
            "focused list uses the accent"
        );
        assert_eq!(border_fg(false), Some(BORDER), "unfocused list stays dim");
    }

    #[test]
    fn focus_panel_uses_accent_border_only_when_focused() {
        let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("").block(focus_panel("worktrees", true)),
                    frame.area(),
                );
            })
            .unwrap();
        let focused = terminal.backend().buffer().clone();
        assert_eq!(
            focused[(0, 0)].style().fg,
            Some(ACCENT),
            "focused border should use the accent"
        );

        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("").block(focus_panel("worktrees", false)),
                    frame.area(),
                );
            })
            .unwrap();
        let idle = terminal.backend().buffer().clone();
        assert_eq!(
            idle[(0, 0)].style().fg,
            Some(BORDER),
            "unfocused border should stay dim"
        );
        // Title colour follows the same rule (find the 'w' of "worktrees").
        let title_x = (0..24u16)
            .find(|&x| idle[(x, 0)].symbol() == "w")
            .expect("title missing");
        assert_eq!(idle[(title_x, 0)].style().fg, Some(BORDER));
        assert_eq!(focused[(title_x, 0)].style().fg, Some(ACCENT));
    }

    /// Three-panel commits chrome must follow Files focus the same way the
    /// changed-file list does: accent border when focused, dim when not.
    #[test]
    fn worktree_commits_panel_uses_focus_chrome() {
        let rows = vec![GraphLine {
            graph: String::new(),
            entry: Some(entry("1a2b3c4", "fix parser", &[])),
        }];
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal
            .draw(|frame| {
                draw_worktree_commits(
                    frame,
                    frame.area(),
                    "main",
                    &rows,
                    &[],
                    0,
                    LogMode::Flat,
                    true,
                    &mut Vec::new(),
                );
            })
            .unwrap();
        let focused = terminal.backend().buffer().clone();
        assert_eq!(
            focused[(0, 0)].style().fg,
            Some(ACCENT),
            "focused commits border should use the accent"
        );

        terminal
            .draw(|frame| {
                draw_worktree_commits(
                    frame,
                    frame.area(),
                    "main",
                    &rows,
                    &[],
                    0,
                    LogMode::Flat,
                    false,
                    &mut Vec::new(),
                );
            })
            .unwrap();
        let idle = terminal.backend().buffer().clone();
        assert_eq!(
            idle[(0, 0)].style().fg,
            Some(BORDER),
            "unfocused commits border should stay dim"
        );
        let title_x = (0..40u16)
            .find(|&x| idle[(x, 0)].symbol() == "c")
            .expect("commits title missing");
        assert_eq!(idle[(title_x, 0)].style().fg, Some(BORDER));
        assert_eq!(focused[(title_x, 0)].style().fg, Some(ACCENT));
    }

    fn candidate(branch: &str) -> CheckoutCandidate {
        CheckoutCandidate {
            branch: branch.to_string(),
            remote: None,
        }
    }

    /// Renders the create dialog with `typed` in the name field. The typed text
    /// doubles as the filter over the checkout candidates, so it decides which
    /// of them are on screen.
    fn create_dialog(typed: &str, selected: usize, base: &str, base_focus: bool) -> Vec<String> {
        let branches = [candidate("feat/login"), candidate("feat/deps")];
        let all = ["main".to_string()];
        let input = super::super::app::TextInput::with_value(typed);
        render(74, 16, |frame, area| {
            draw_create_dialog(
                frame,
                area,
                &input,
                &branches,
                &all,
                base,
                selected,
                base_focus,
                None,
                Some("~/Dev/wt"),
            );
        })
    }

    #[test]
    fn truncate_middle_keeps_both_ends() {
        assert_eq!(truncate_middle("main", 20), "main");
        assert_eq!(truncate_middle("main", 4), "main", "exact fit is untouched");
        let out = truncate_middle("release/2026-q1-hotfix-rollup", 16);
        assert_eq!(out.chars().count(), 16, "{out}");
        assert!(out.starts_with("release/"), "keeps the namespace: {out}");
        assert!(out.ends_with("llup"), "keeps the tail: {out}");
        assert!(out.contains('…'), "{out}");
    }

    #[test]
    fn truncate_middle_handles_multibyte_and_tiny_budgets() {
        // Splitting by chars, not bytes, so an accented name can't panic.
        let out = truncate_middle("fix/café-señor-branch", 10);
        assert_eq!(out.chars().count(), 10, "{out}");
        assert_eq!(truncate_middle("anything", 1), "…");
    }

    /// The path budget `draw_list` computes must match the width ratatui
    /// actually hands the PATH column, or the front-truncated path gets clipped
    /// at the back again. Rebuilds the same table and counts the cell.
    #[test]
    fn path_column_budget_matches_the_rendered_width() {
        let width = 100u16;
        let name_w = 12u16;
        let block = focus_panel("worktrees", true);
        let inner = block.inner(Rect::new(0, 0, width, 4));
        let budget = inner.width.saturating_sub(
            HIGHLIGHT_SYMBOL_W + name_w + CHANGES_W + UPSTREAM_W + FLAGS_W + COLUMN_SPACING * 4,
        ) as usize;
        let filler = "x".repeat(200);
        let out = render(width, 4, |frame, area| {
            let table = Table::new(
                vec![Row::new(vec![
                    Cell::from("wt"),
                    Cell::from("clean"),
                    Cell::from("-"),
                    Cell::from(""),
                    Cell::from(filler.clone()),
                ])],
                [
                    Constraint::Length(name_w),
                    Constraint::Length(CHANGES_W),
                    Constraint::Length(UPSTREAM_W),
                    Constraint::Length(FLAGS_W),
                    Constraint::Min(MIN_PATH_W),
                ],
            )
            .column_spacing(COLUMN_SPACING)
            .header(Row::new(["NAME", "CHANGES", "UPSTREAM", "FLAGS", "PATH"]))
            .block(block)
            .highlight_symbol(Span::raw("▌ "));
            let mut state = TableState::default().with_selected(Some(0));
            frame.render_stateful_widget(table, area, &mut state);
        });
        let drawn = out[2].chars().filter(|c| *c == 'x').count();
        assert_eq!(drawn, budget, "{out:#?}");
    }

    /// The PATH column drops the shared prefix, never the worktree directory
    /// at the end, which is the part that identifies the row.
    #[test]
    fn truncate_start_keeps_the_tail() {
        assert_eq!(truncate_start("/Users/w/Dev/wtm", 20), "/Users/w/Dev/wtm");
        assert_eq!(
            truncate_start("/Users/w/Dev/wtm", 16),
            "/Users/w/Dev/wtm",
            "exact fit is untouched"
        );
        let out = truncate_start("/Users/w/Dev/wtm-worktrees/feat-login", 20);
        assert_eq!(out.chars().count(), 20, "{out}");
        assert!(out.starts_with('…'), "{out}");
        assert!(out.ends_with("feat-login"), "{out}");
    }

    #[test]
    fn truncate_start_handles_multibyte_and_tiny_budgets() {
        // Splitting by chars, not bytes, so an accented path can't panic.
        let out = truncate_start("/Users/w/Dev/café-señor", 10);
        assert_eq!(out.chars().count(), 10, "{out}");
        assert!(out.ends_with("afé-señor"), "{out}");
        assert_eq!(truncate_start("anything", 1), "…");
        assert_eq!(truncate_start("anything", 0), "");
    }

    fn detail_branch(name: &str, path: Option<&str>, remote: Option<&str>) -> BranchListItem {
        BranchListItem {
            name: name.to_string(),
            checked_out_path: path.map(str::to_string),
            upstream: None,
            ahead: 0,
            behind: 0,
            subject: String::new(),
            date: String::new(),
            merged: false,
            remote: remote.map(str::to_string),
            created_from: None,
            changed_from_base: false,
            behind_base: false,
            archived: false,
        }
    }

    /// Rendering of a `Line` as plain text, for asserting on the detail line.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The detail line carries the two values the fixed-width columns elide:
    /// the branch's full name and the full path it is checked out at.
    #[test]
    fn branch_detail_line_shows_full_name_and_path() {
        let b = detail_branch(
            "feature/some-quite-long-branch-name",
            Some("/Users/w/Dev/proj-worktrees/feature-some-quite-long-branch-name"),
            None,
        );
        let text = line_text(&branch_detail_line(&b, 160));
        assert!(
            text.contains("feature/some-quite-long-branch-name"),
            "{text}"
        );
        assert!(
            text.contains("/Users/w/Dev/proj-worktrees/feature-some-quite-long-branch-name"),
            "{text}"
        );
        assert!(
            !text.contains('…'),
            "nothing is elided when it fits: {text}"
        );
    }

    /// Too narrow to hold both: the path gives way from the front (its tail is
    /// the identifying half) while the name is kept whole.
    #[test]
    fn branch_detail_line_trims_the_path_before_the_name() {
        let b = detail_branch(
            "feature/login",
            Some("/Users/w/Dev/proj-worktrees/feature-login"),
            None,
        );
        let line = branch_detail_line(&b, 40);
        let text = line_text(&line);
        assert!(text.chars().count() <= 40, "{text}");
        assert!(text.contains("feature/login"), "name is whole: {text}");
        assert!(text.contains('…'), "path is elided: {text}");
        assert!(text.ends_with("feature-login"), "tail survives: {text}");
    }

    /// Past the path's floor the name itself is elided rather than the line
    /// overflowing, and a branch with no checkout says so instead of showing a
    /// blank half.
    #[test]
    fn branch_detail_line_handles_tiny_widths_and_no_checkout() {
        let long = detail_branch(
            "feature/a-really-very-extremely-long-branch-name",
            Some("/Users/w/Dev/proj-worktrees/x"),
            None,
        );
        let text = line_text(&branch_detail_line(&long, 30));
        assert!(text.chars().count() <= 30, "{text}");
        assert!(text.contains('…'), "{text}");

        let local = detail_branch("main", None, None);
        assert!(
            line_text(&branch_detail_line(&local, 80)).contains("not checked out"),
            "a local branch with no worktree says so"
        );
        let remote = detail_branch("wip", None, Some("origin/wip"));
        assert!(
            line_text(&branch_detail_line(&remote, 80)).contains("on origin/wip only"),
            "a remote-only branch names its remote"
        );
    }

    /// The base branch gets its own row under the name input, so a long branch
    /// name can never crowd the field being typed into.
    #[test]
    fn create_dialog_puts_the_base_on_its_own_row() {
        let out = create_dialog("my-feature", 0, "main", false);
        let name_row = out.iter().position(|l| l.contains("my-feature")).unwrap();
        let base_row = out.iter().position(|l| l.contains("↳ off")).unwrap();
        assert_eq!(base_row, name_row + 1, "base sits directly below: {out:#?}");
        assert!(out[base_row].contains("main"), "{out:#?}");
        assert!(out[base_row].contains('⌄'), "dropdown affordance: {out:#?}");
        assert!(out[base_row].contains("⇥ Tab"), "hint is inline: {out:#?}");
    }

    /// A base branch too long for its row is elided in the middle rather than
    /// pushing the dialog out of shape.
    #[test]
    fn create_dialog_truncates_a_long_base_branch() {
        let long = "release/2026-q1-hotfix-rollup-candidate-for-the-november-train";
        let out = create_dialog("my-feature", 0, long, false);
        let base_row = out.iter().find(|l| l.contains("↳ off")).unwrap();
        assert!(base_row.contains('…'), "elided: {base_row}");
        assert!(
            base_row.contains("release/"),
            "keeps the namespace: {base_row}"
        );
        assert!(base_row.contains("train"), "keeps the tail: {base_row}");
        // The dialog is a fixed 66 columns inside a 74-wide screen; the row must
        // stay within it rather than bleeding across the border.
        assert!(base_row.chars().count() <= 74, "{base_row}");
        assert!(
            base_row.trim_end().ends_with('┃'),
            "border intact: {base_row}"
        );
    }

    /// The section header and the blank spacer under it are both
    /// non-selectable, so the highlight has to skip the pair. Off-by-one here
    /// would highlight the row above the branch the user is actually on.
    #[test]
    fn create_dialog_highlights_the_selected_branch_past_the_spacer() {
        let out = create_dialog("feat", 1, "main", false);
        let header = out
            .iter()
            .position(|l| l.contains("or check out a match"))
            .unwrap();
        // A blank spacer separates the header from the candidates; the row
        // holds nothing but the panel's own borders.
        assert!(
            out[header + 1]
                .chars()
                .all(|c| c == '┃' || c == ' ' || c == '┏' || c == '┗'),
            "a blank spacer follows the header: {out:#?}"
        );
        // `▌` is the highlight symbol; it must land on the first candidate.
        let marked = out.iter().find(|l| l.contains('▌')).unwrap();
        assert!(marked.contains("feat/login"), "{out:#?}");

        let out2 = create_dialog("feat", 2, "main", false);
        let marked2 = out2.iter().find(|l| l.contains('▌')).unwrap();
        assert!(marked2.contains("feat/deps"), "{out2:#?}");
    }

    /// A subject longer than the field must scroll with the cursor: the tail
    /// stays visible and `‹` marks what scrolled off, instead of the cursor
    /// running off the right edge.
    #[test]
    fn prompt_line_windowed_follows_the_cursor() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let width = 12;
        let at = |cursor: usize| {
            render(width, 1, |frame, area| {
                frame.render_widget(
                    Paragraph::new(prompt_line_windowed(text, cursor, width)),
                    area,
                );
            })
            .remove(0)
        };

        let end = at(text.chars().count());
        assert!(end.contains('‹'), "text scrolled off the left: {end}");
        assert!(end.contains('z'), "the cursor end must be visible: {end}");
        assert!(
            !end.contains('a'),
            "the head must have scrolled away: {end}"
        );

        let start = at(0);
        assert!(start.contains('a'), "{start}");
        assert!(
            start.contains('›'),
            "text continues past the right: {start}"
        );
        assert!(!start.contains('z'), "{start}");
    }

    /// Short values are untouched: no markers, no window.
    #[test]
    fn prompt_line_windowed_leaves_short_values_alone() {
        let out = render(20, 1, |frame, area| {
            frame.render_widget(Paragraph::new(prompt_line_windowed("hi", 2, 20)), area);
        })
        .remove(0);
        assert_eq!(out, "❯ hi▏");
    }

    /// The finished-create banner is pinned, so a setup run that produced more
    /// output than the popup can hold still ends with a visible "ready".
    #[test]
    fn creating_banner_survives_a_long_log() {
        let lines: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
        let outcome = CreateOutcome {
            ok: true,
            path: "/tmp/wt/feature".to_string(),
            detail: None,
        };
        let out = render(90, 20, |frame, area| {
            draw_creating(frame, area, "feature", &lines, Some(&outcome), "", false);
        });
        assert!(out.iter().any(|r| r.contains("READY")), "{out:#?}");
        assert!(
            out.iter().any(|r| r.contains("/tmp/wt/feature")),
            "{out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("press Enter to continue")),
            "{out:#?}"
        );
        // The log is still there, tailing.
        assert!(out.iter().any(|r| r.contains("line 199")), "{out:#?}");
    }

    /// A failed create says so in the same pinned spot, with the error.
    #[test]
    fn creating_banner_reports_failure() {
        let outcome = CreateOutcome {
            ok: false,
            path: String::new(),
            detail: Some("branch already checked out".to_string()),
        };
        let out = render(90, 12, |frame, area| {
            draw_creating(frame, area, "feature", &[], Some(&outcome), "", false);
        });
        assert!(out.iter().any(|r| r.contains("FAILED")), "{out:#?}");
        assert!(
            out.iter().any(|r| r.contains("branch already checked out")),
            "{out:#?}"
        );
    }

    /// A git-add failure must put the diagnostic on the first screen, not hide
    /// it behind a wrapped `git add -- file1 file2 …` command line.
    #[test]
    fn error_popup_leads_with_gits_diagnostic() {
        let msg = crate::git::GitError::Command {
            args: "add -- a.rs b.rs c.rs d.rs e.rs f.rs".to_string(),
            stderr: "fatal: pathspec 'a.rs' did not match any files".to_string(),
        }
        .to_string();
        let out = render(80, 20, |frame, area| {
            draw_error_popup(frame, area, &msg, 0);
        });
        let screen = out.join("\n");
        let fatal_row = out.iter().position(|r| r.contains("fatal: pathspec"));
        let cmd_row = out.iter().position(|r| r.contains("from `git"));
        assert!(fatal_row.is_some(), "diagnostic missing:\n{screen}");
        assert!(
            fatal_row <= cmd_row,
            "diagnostic must be above the command:\n{screen}"
        );
    }

    /// The commit dialog shows the body field, with its content.
    #[test]
    fn commit_dialog_draws_the_body_field() {
        let files = vec![StatusEntry {
            code: "M ".to_string(),
            path: "src/main.rs".to_string(),
        }];
        let body = super::super::app::TextArea::new("why this change\nsecond line");
        let out = render(100, 30, |frame, area| {
            draw_commit(
                frame,
                area,
                "feature",
                &files,
                &[true],
                0,
                &TextInput::with_value("subject"),
                &body,
                &CommitFocus::Body,
                false,
            );
        });
        assert!(
            out.iter().any(|r| r.contains("Body (optional)")),
            "{out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("why this change")),
            "{out:#?}"
        );
        assert!(out.iter().any(|r| r.contains("second line")), "{out:#?}");
        assert!(out.iter().any(|r| r.contains("^S commits")), "{out:#?}");
    }

    /// More files than the popup's 10-row window still reach the screen: the
    /// list scrolls so the cursor file is visible, and the hint counts them all.
    #[test]
    fn commit_dialog_scrolls_the_file_list_to_the_cursor() {
        let files: Vec<StatusEntry> = (0..15)
            .map(|i| StatusEntry {
                code: "M ".to_string(),
                path: format!("src/file{i:02}.rs"),
            })
            .collect();
        let marked = vec![true; files.len()];
        let out = render(100, 30, |frame, area| {
            draw_commit(
                frame,
                area,
                "feature",
                &files,
                &marked,
                14,
                &TextInput::with_value("subject"),
                &super::super::app::TextArea::default(),
                &CommitFocus::Files,
                false,
            );
        });
        let screen = out.join("\n");
        assert!(
            out.iter().any(|r| r.contains("src/file14.rs")),
            "cursor file must be on screen:\n{screen}"
        );
        assert!(
            out.iter().all(|r| !r.contains("src/file00.rs")),
            "list must have scrolled past the first file:\n{screen}"
        );
        assert!(
            out.iter().any(|r| r.contains("15/15")),
            "hint still counts every file:\n{screen}"
        );
    }

    /// On a terminal too short for the whole dialog the body box gives way
    /// first; the subject line and the hint must never be squeezed out.
    #[test]
    fn commit_dialog_survives_a_short_terminal() {
        let files: Vec<StatusEntry> = (0..10)
            .map(|i| StatusEntry {
                code: "M ".to_string(),
                path: format!("src/file{i}.rs"),
            })
            .collect();
        let marked = vec![true; files.len()];
        let out = render(100, 16, |frame, area| {
            draw_commit(
                frame,
                area,
                "feature",
                &files,
                &marked,
                0,
                &TextInput::with_value("subject here"),
                &super::super::app::TextArea::default(),
                &CommitFocus::Message,
                false,
            );
        });
        assert!(out.iter().any(|r| r.contains("Commit message")), "{out:#?}");
        assert!(out.iter().any(|r| r.contains("subject here")), "{out:#?}");
        assert!(out.iter().any(|r| r.contains("^S commits")), "{out:#?}");
    }

    /// A two-hunk conflicted file, for the resolver rendering tests.
    fn resolver_file(actions: Vec<Option<ResolutionAction>>, hunk: usize) -> ResolverFile {
        ResolverFile {
            file: crate::ops::ConflictFile {
                path: "src/main.rs".into(),
                segments: vec![
                    ConflictSegment::Plain("fn main() {\n".into()),
                    ConflictSegment::Hunk {
                        ours: "    mine();\n".into(),
                        theirs: "    yours();\n".into(),
                        base: None,
                    },
                    ConflictSegment::Plain("}\n".into()),
                    ConflictSegment::Hunk {
                        ours: "    mine2();\n".into(),
                        theirs: "    yours2();\n".into(),
                        base: None,
                    },
                ],
                ours_label: "main".into(),
                theirs_label: "feature/login".into(),
            },
            actions,
            hunk,
        }
    }

    /// Renders the resolver over a two-hunk file, at a width that keeps the
    /// stacked (narrow) layout.
    fn render_resolver(kind: ResolveKind, rf: &ResolverFile) -> Vec<String> {
        render_resolver_at(96, 30, kind, rf, 0, true)
    }

    /// Renders the resolver at an explicit size and scroll position.
    fn render_resolver_at(
        width: u16,
        height: u16,
        kind: ResolveKind,
        rf: &ResolverFile,
        scroll: u16,
        follow: bool,
    ) -> Vec<String> {
        render(width, height, |frame, area| {
            draw_conflict_resolver(
                frame,
                area,
                "wt",
                "feature/login",
                &kind,
                &["src/main.rs".to_string()],
                &[false],
                0,
                Some(rf),
                None,
                scroll,
                follow,
                true,
            );
        })
    }

    /// The old pane put the OURS/THEIRS key in a header that scrolled away with
    /// the first hunk, leaving later hunks as two anonymous blocks of text. Now
    /// every hunk names both sides, the branch each came from, and the key that
    /// takes it, so no hunk can be read out of context.
    #[test]
    fn resolver_names_both_sides_on_every_hunk() {
        let rf = resolver_file(vec![None, None], 0);
        let out = render_resolver(ResolveKind::Merge, &rf);
        // Only the rows inside a hunk block (the ones opened by a box-drawing
        // corner), not the pane's legend.
        let ours = out
            .iter()
            .filter(|r| r.contains("┌") && r.contains("OURS · main") && r.contains("[o]"))
            .count();
        let theirs = out
            .iter()
            .filter(|r| {
                r.contains("├") && r.contains("THEIRS · feature/login") && r.contains("[t]")
            })
            .count();
        assert_eq!(ours, 2, "both hunks label ours: {out:#?}");
        assert_eq!(theirs, 2, "both hunks label theirs: {out:#?}");
        assert!(
            out.iter()
                .any(|r| r.contains("OURS · main") && r.contains("already in this worktree")),
            "the legend says where ours came from: {out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("THEIRS · feature/login")
                    && r.contains("incoming from the merge")),
            "the legend says where theirs came from: {out:#?}"
        );
    }

    /// An undecided hunk advertises every way out of it, including keeping both
    /// sides and editing the whole file.
    #[test]
    fn resolver_offers_both_and_the_editors_on_screen() {
        let rf = resolver_file(vec![None, None], 0);
        let out = render_resolver(ResolveKind::Merge, &rf);
        assert!(
            out.iter()
                .any(|r| r.contains("undecided — press o / t / b")),
            "{out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("b both") && r.contains("⇧B both, theirs 1st")),
            "keeping both is offered per hunk: {out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("0/2 decided") && r.contains("⇧E edit file")),
            "the sticky footer counts the decisions and offers the file editor: {out:#?}"
        );
    }

    /// The decision has to be readable off the hunk itself: the kept side is
    /// marked "keep", the discarded one "drop".
    #[test]
    fn resolver_marks_the_kept_side_and_the_dropped_one() {
        let rf = resolver_file(vec![Some(ResolutionAction::KeepTheirs), None], 0);
        let out = render_resolver(ResolveKind::Merge, &rf);
        assert!(
            out.iter()
                .any(|r| r.contains("✗ drop") && r.contains("OURS · main")),
            "ours is dropped: {out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("✓ keep") && r.contains("THEIRS · feature/login")),
            "theirs is kept: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("keeping THEIRS")),
            "and the hunk header says so: {out:#?}"
        );
    }

    /// Keeping both sides shows the order they will be written in, so "both"
    /// isn't a guess about what lands in the file.
    #[test]
    fn resolver_numbers_both_sides_when_both_are_kept() {
        let rf = resolver_file(vec![Some(ResolutionAction::KeepBoth), None], 0);
        let out = render_resolver(ResolveKind::Merge, &rf);
        assert!(
            out.iter()
                .any(|r| r.contains("✓ keep 1st") && r.contains("OURS")),
            "{out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("✓ keep 2nd") && r.contains("THEIRS")),
            "{out:#?}"
        );

        let rev = resolver_file(vec![Some(ResolutionAction::KeepBothReversed), None], 0);
        let out = render_resolver(ResolveKind::Merge, &rev);
        assert!(
            out.iter()
                .any(|r| r.contains("✓ keep 2nd") && r.contains("OURS")),
            "reversed puts theirs first: {out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("✓ keep 1st") && r.contains("THEIRS")),
            "reversed puts theirs first: {out:#?}"
        );
    }

    /// Each side is numbered from 1 so the two line up for comparison: "line 2
    /// here" against "line 2 there" is the reading being made, which file
    /// offsets would break by putting the same logical line on two numbers.
    #[test]
    fn resolver_numbers_each_side_from_one() {
        let rf = ResolverFile {
            file: crate::ops::ConflictFile {
                path: "src/main.rs".into(),
                segments: vec![ConflictSegment::Hunk {
                    ours: "    a();\n    b();\n".into(),
                    theirs: "    x();\n    y();\n".into(),
                    base: None,
                }],
                ours_label: "main".into(),
                theirs_label: "feature/login".into(),
            },
            actions: vec![None],
            hunk: 0,
        };
        let out = render_resolver(ResolveKind::Merge, &rf);
        for (n, code) in [(1, "a();"), (2, "b();"), (1, "x();"), (2, "y();")] {
            assert!(
                out.iter()
                    .any(|r| r.contains(&format!("{n} │ ")) && r.contains(code)),
                "{code} is numbered {n} on its own side: {out:#?}"
            );
        }
    }

    /// Choosing a side records a choice and writes nothing. The pane says so in
    /// the legend, which never scrolls away, so a hunk marked "keeping BOTH"
    /// can't look finished while git still sees the path as unmerged.
    #[test]
    fn resolver_warns_that_choices_are_not_written_yet() {
        let undecided = resolver_file(vec![None, None], 0);
        let out = render_resolver(ResolveKind::Merge, &undecided);
        assert!(
            !out.iter().any(|r| r.contains("not written yet")),
            "nothing chosen, nothing to warn about: {out:#?}"
        );

        let chosen = resolver_file(vec![Some(ResolutionAction::KeepBoth), None], 0);
        let out = render_resolver(ResolveKind::Merge, &chosen);
        assert!(
            out.iter()
                .any(|r| r.contains("1 of 2 choice(s) not written yet")),
            "the pending write is spelled out: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("saves this file")),
            "and says which key does it: {out:#?}"
        );
    }

    /// The conflict editor takes the whole frame and scrolls sideways with the
    /// cursor, so a long line is readable instead of clipped at the border.
    #[test]
    fn file_editor_fills_the_frame_and_scrolls_horizontally() {
        let long = format!("let x = {};", "z".repeat(200));
        let mut editor = super::super::app::TextArea::new(&format!("short\n{long}\ntail\n"));

        let out = render(60, 20, |frame, area| {
            draw_file_editor(frame, area, "src/main.rs", &editor);
        });
        assert!(
            out.iter().any(|r| r.contains("src/main.rs")),
            "the editor is on screen: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("1 │") || r.contains("1 ")),
            "line numbers are drawn: {out:#?}"
        );
        // Full frame: the panel border opens on the very first column of the
        // very first row rather than being inset inside a centred popup.
        assert!(
            out[0].starts_with('┏') && out[0].ends_with('┓'),
            "the editor spans the whole width: {:?}",
            out[0]
        );
        assert_eq!(out.len(), 20);

        // Park the cursor far along the long line; that column must be visible.
        editor.row = 1;
        editor.col = 190;
        let out = render(60, 20, |frame, area| {
            draw_file_editor(frame, area, "src/main.rs", &editor);
        });
        assert!(
            out.iter().any(|r| r.contains("zzz")),
            "the view followed the cursor into the line: {out:#?}"
        );
        assert!(
            !out.iter().any(|r| r.contains("let x =")),
            "and scrolled the start of the line off to the left: {out:#?}"
        );
    }

    /// A rebase swaps git's sides, which is the one thing a hunk can't say for
    /// itself, so the warning gets a row that never scrolls away.
    #[test]
    fn resolver_warns_that_a_rebase_swaps_the_sides() {
        let rf = resolver_file(vec![None, None], 1);
        let out = render_resolver(ResolveKind::Rebase, &rf);
        assert!(
            out.iter().any(|r| r.contains("a rebase swaps the sides")),
            "{out:#?}"
        );
        assert!(
            out.iter()
                .any(|r| r.contains("the branch you're rebasing onto")),
            "ours is described as the rebase target: {out:#?}"
        );
    }

    /// A pane wide enough for it lays each hunk out the way every other merge
    /// tool does: the incoming side on the left, what will be written in the
    /// middle, the current side on the right, arrows pointing inwards.
    #[test]
    fn resolver_lays_wide_hunks_out_as_theirs_final_ours() {
        let rf = resolver_file(vec![None, None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &rf, 0, true);
        let header = out
            .iter()
            .find(|r| r.contains("FINAL"))
            .unwrap_or_else(|| panic!("no three-column header: {out:#?}"));
        let theirs = header.find("THEIRS").expect(header);
        let mid = header.find("FINAL").expect(header);
        let ours = header.find("OURS").expect(header);
        assert!(
            theirs < mid && mid < ours,
            "theirs → final ← ours, left to right: {header}"
        );
        assert!(header.contains('▶') && header.contains('◀'), "{header}");
    }

    /// Picking a side rewrites FINAL, which is the whole point of showing it:
    /// the decision is visible as the text it produces, not as a word.
    #[test]
    fn resolver_final_column_shows_the_chosen_side() {
        let theirs = resolver_file(vec![Some(ResolutionAction::KeepTheirs), None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &theirs, 0, true);
        // The row carrying the first hunk's two sides also carries FINAL, so
        // "yours" appearing twice on it is theirs *and* the result.
        let row = out
            .iter()
            .find(|r| r.contains("yours();"))
            .unwrap_or_else(|| panic!("{out:#?}"));
        assert_eq!(row.matches("yours();").count(), 2, "theirs → final: {row}");
        assert_eq!(row.matches("mine();").count(), 1, "ours still shown: {row}");

        let ours = resolver_file(vec![Some(ResolutionAction::KeepOurs), None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &ours, 0, true);
        let row = out
            .iter()
            .find(|r| r.contains("mine();"))
            .unwrap_or_else(|| panic!("{out:#?}"));
        assert_eq!(row.matches("mine();").count(), 2, "ours → final: {row}");
    }

    /// Keeping both is a mix, and FINAL shows it as one: two lines, in the
    /// order they will be written.
    #[test]
    fn resolver_final_column_stacks_both_sides_when_mixing() {
        let rf = resolver_file(vec![Some(ResolutionAction::KeepBoth), None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &rf, 0, true);
        let first = out.iter().position(|r| r.contains("mine();")).unwrap();
        // Ours first, then theirs on the row below it, both inside FINAL.
        let second = out
            .iter()
            .skip(first + 1)
            .position(|r| r.contains("yours();"))
            .map(|i| i + first + 1)
            .unwrap_or_else(|| panic!("{out:#?}"));
        assert!(second > first, "ours then theirs: {out:#?}");
        let reversed = resolver_file(vec![Some(ResolutionAction::KeepBothReversed), None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &reversed, 0, true);
        let row = out
            .iter()
            .find(|r| r.contains("yours();"))
            .unwrap_or_else(|| panic!("{out:#?}"));
        assert_eq!(
            row.matches("yours();").count(),
            2,
            "theirs leads the mix: {row}"
        );
    }

    /// An empty FINAL would read as "this hunk is fine as it is", which is the
    /// opposite of the truth, so it says what it is waiting for.
    #[test]
    fn resolver_final_column_says_when_nothing_is_chosen() {
        let rf = resolver_file(vec![None, None], 0);
        let out = render_resolver_at(150, 30, ResolveKind::Merge, &rf, 0, true);
        assert!(out.iter().any(|r| r.contains("nothing chosen")), "{out:#?}");
    }

    /// A narrow pane can't fit three columns of code, so it keeps the stacked
    /// blocks rather than shredding every line.
    #[test]
    fn resolver_keeps_stacked_blocks_on_a_narrow_pane() {
        let rf = resolver_file(vec![None, None], 0);
        let out = render_resolver_at(90, 30, ResolveKind::Merge, &rf, 0, true);
        assert!(
            !out.iter().any(|r| r.contains("FINAL")),
            "no columns this narrow: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("keep") || r.contains("OURS")),
            "{out:#?}"
        );
    }

    /// The pane scrolls on its own. It used to be pinned to the current hunk,
    /// so anything below the last hunk's first screenful was unreachable.
    #[test]
    fn resolver_body_scrolls_independently_of_the_hunk_cursor() {
        let rf = resolver_file(vec![None, None], 0);
        let top = render_resolver_at(150, 12, ResolveKind::Merge, &rf, 0, false);
        let down = render_resolver_at(150, 12, ResolveKind::Merge, &rf, 4, false);
        assert_ne!(top, down, "a scrolled pane shows different lines");
        assert!(
            down.iter().any(|r| r.contains("hunk 2 of 2")),
            "scrolling reaches the second hunk with the cursor still on the \
             first: {down:#?}"
        );
    }

    /// The whole-file editor numbers the lines and keeps git's markers visible,
    /// since removing them is the job it exists for.
    #[test]
    fn file_editor_numbers_lines_and_keeps_the_markers() {
        let editor = super::super::app::TextArea::new(
            "fn main() {\n<<<<<<< main\n    mine();\n=======\n    yours();\n>>>>>>> feature\n}\n",
        );
        let out = render(80, 14, |frame, area| {
            draw_file_editor(frame, area, "src/main.rs", &editor);
        });
        assert!(
            out.iter().any(|r| r.contains("edit src/main.rs")),
            "the panel names the file: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("2 <<<<<<< main")),
            "numbered, markers intact: {out:#?}"
        );
        assert!(
            out.iter().any(|r| r.contains("6 >>>>>>> feature")),
            "numbered, markers intact: {out:#?}"
        );
    }
}
