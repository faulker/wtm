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
    App, CheckoutCandidate, CherryTarget, CommitFocus, ConfirmOption, DiffRow, LogMode, Modal,
    ResolverFile, RowList, Tab, TextInput, View, WorktreesFocus, filtered_candidates,
};
use super::config_editor::{
    CHECK_ROW, ConfigEditor, FIELD_ROWS, FORM_LINES, LAYOUT_ROW, OPEN_COMMAND_ROW,
    OpenCommandEditor, THEME_ROW, UPDATE_ROW,
};
use super::help::{self, Binding, HelpTab};
use super::highlight;
use super::setup::{
    REVIEW_ROWS, SetupWizard, Step, WELCOME_OPTIONS, location_label, location_preview,
};
use super::theme::{self, ACCENT, BORDER, GRAPH_COLORS, SELECTION_BG};
use crate::config::{
    DEFAULT_AUTO_UPDATE_CHECK, DEFAULT_DIFF_THEME, DEFAULT_LOCATION, LOCATION_PRESETS,
    OpenCommandVars, WorktreesLayout, expand_open_command, worktrees_layout_label,
};
use crate::conflict::{ConflictSegment, ResolutionAction};
use crate::git::{GraphLine, StatusEntry};
use crate::ops::ResolveKind;
use crate::update::{CURRENT_VERSION, Release};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    // Click targets are re-recorded from scratch each frame by whoever draws
    // them, so last frame's geometry can't outlive what's on screen.
    app.tab_hits.clear();
    app.preview_list = None;
    app.files_list = None;
    app.diff_path_hit = None;
    // The full-screen view's clickable list, if any.
    let list_hit = match &app.view {
        View::Log {
            name,
            lines,
            selected,
        } => draw_log(frame, main, name, lines, *selected, app.log_mode),
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
        ),
        View::BranchCommits {
            branch,
            lines,
            marked,
            selected,
        } => draw_branch_commits(frame, main, branch, lines, marked, *selected, app.log_mode),
        View::ConflictResolver {
            target,
            source_label,
            kind,
            files,
            resolved,
            file,
            current,
            ..
        } => draw_conflict_resolver(
            frame,
            main,
            target,
            source_label,
            kind,
            files,
            resolved,
            *file,
            current.as_ref(),
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
            done,
            input,
            kill_armed,
            ..
        } => draw_creating(frame, main, branch, lines, *done, input, *kill_armed),
        View::Setup(wizard) => overlay_hit = draw_setup(frame, main, wizard),
        View::Commit {
            name,
            files,
            marked,
            cursor,
            input,
            focus,
        } => overlay_hit = draw_commit(frame, main, name, files, marked, *cursor, input, focus),
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
    // Only Confirm reports its own rows; Prompt/HunkEditor have none.
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
        | View::BranchCommits { .. }
        | View::ConflictResolver { .. } => list_hit,
        View::Commit { .. }
        | View::Switch { .. }
        | View::CherryPick { .. }
        | View::MergePick { .. }
        | View::MoveChanges { .. }
        | View::OpenCommand { .. }
        | View::StashTarget { .. }
        | View::Setup(_) => overlay_hit,
        _ => None,
    };
    // A modal covers the list, so clicks go to it instead when it has its own
    // rows (Confirm); other modal kinds (Prompt, HunkEditor) just suppress
    // clicks on whatever is behind them.
    if let Some(modal) = &app.modal {
        app.row_list = matches!(modal, Modal::Confirm { .. })
            .then_some(modal_hit)
            .flatten();
    }

    // The tab bar and the worktree preview only take clicks when the home view
    // is what the user is actually looking at: a dialog, overlay, or modal
    // floats over both, and clicks belong to whatever is on top.
    if !matches!(app.view, View::List) || app.modal.is_some() {
        app.tab_hits.clear();
        app.preview_list = None;
    }

    // The error popup sits on top of absolutely everything, including the
    // help overlay, and suppresses clicks on whatever is behind it. Cloned so
    // drawing it doesn't hold an immutable borrow while `row_list` is reset.
    if let Some(err) = app.error.clone() {
        draw_error_popup(frame, main, &err);
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

/// Panel chrome for a focusable list: accent border and bold title when it
/// owns the keyboard, dim border and title when another panel does. Diff
/// panes and other non-focus targets keep using [`panel`].
fn focus_panel(title: impl Into<String>, focused: bool) -> Block<'static> {
    let (border, title_style) = if focused {
        (
            ACCENT,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
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
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let count = app.worktrees.len();
    let path = app
        .worktrees
        .get(app.selected)
        .map(|wt| wt.path.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| app.ctx.repo_root.display().to_string());
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

/// The worktree table. `focused` is false when another panel owns the keyboard
/// (the three-panel layout's file list), which dims the row highlight so it is
/// clear which cursor the arrow keys move.
fn draw_list(frame: &mut Frame, area: Rect, app: &mut App, focused: bool) -> Option<RowList> {
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
            let changes = if wt.dirty > 0 {
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
            // Flags: upstream sync, base-relative status, then cleanup/lock.
            let flag_spans = status_flag_spans(&wt.flag_labels());
            Row::new(vec![
                Cell::from(name),
                Cell::from(changes),
                Cell::from(upstream),
                Cell::from(Line::from(flag_spans)),
                Cell::from(Span::styled(wt.path.clone(), Style::new().dim())),
            ])
        })
        .collect();

    let name_w = app
        .worktrees
        .iter()
        .map(|w| w.name.len() + 2)
        .max()
        .unwrap_or(10)
        .max(10) as u16;
    let block = focus_panel("worktrees", focused);
    let inner = block.inner(area);
    let table = Table::new(
        rows,
        [
            Constraint::Length(name_w),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(28),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["NAME", "CHANGES", "UPSTREAM", "FLAGS", "PATH"]).style(Style::new().dim().bold()),
    )
    .block(block)
    .row_highlight_style(highlight_style(focused))
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(cursor_color(focused))));
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
    // The table header occupies the first inner row, so data rows start one
    // line below it.
    Some(RowList {
        inner,
        header: 1,
        offset: state.offset(),
        len: app.worktrees.len(),
    })
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
    if let Some(panel) = &app.worktree_commits {
        app.diff_path_hit = None;
        app.files_list = draw_worktree_commits(
            frame,
            changes_area,
            &panel.branch,
            &panel.lines,
            panel.selected,
            app.log_mode,
            files_focused,
        );
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
                highlight::diff_lines(path, content)
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
    // footer only shows how to dismiss it.
    if app.error.is_some() {
        frame.render_widget(
            Paragraph::new(hint_line_fitting(
                &[hint("any key", "dismiss error")],
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
    let hints: &[Binding] = match &app.view {
        View::List => match app.tab {
            // Three-panel layout: the hints follow the focused panel, since the
            // same keys act on a different list in each.
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
                hint("[ done ]", "save"),
                hint("Esc", "discard"),
            ],
            Tab::Settings => help::SETTINGS,
        },
        View::Log { .. } => &[
            hint("↑/↓", "commit"),
            hint("Enter", "browse files"),
            hint("g", "top"),
            hint("t", "tree/flat"),
            hint("q", "back"),
        ],
        View::CommitDiff { .. } | View::StashDiff { .. } => &[
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
        View::StashTarget { .. } => &[
            hint("↑/↓", "pick worktree"),
            hint("Enter", "apply"),
            hint("Esc", "cancel"),
        ],
        View::ConflictResolver { .. } => help::RESOLVER,
        View::Commit { focus, .. } => match focus {
            CommitFocus::Files => help::COMMIT_FILES,
            CommitFocus::Message => &[
                hint("type", "commit message"),
                hint("Tab", "pick files"),
                hint("Enter", "commit"),
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
        View::Creating { .. } => &[hint("Enter", "continue")],
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
    // Rows: the "new branch" action, a section header (only when there are
    // branches to check out), then one row per matching existing branch.
    let header_rows = usize::from(!filtered.is_empty());
    let list_rows = (1 + header_rows + filtered.len()).min(10) as u16;
    let popup = centered(area, 66, 7 + list_rows);
    frame.render_widget(Clear, popup);
    frame.render_widget(panel("new worktree"), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [name_area, list_area, base_hint_area, loc_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(list_rows + 1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(prompt_line_at(name.as_str(), name.cursor)),
        name_area,
    );

    // The `[ Base: <branch> ⌄ ]` button: filled and bold when focused, otherwise
    // a bracketed accent chip. The ⌄ signals it opens a dropdown of branches.
    let button_style = if base_focus {
        Style::new().fg(Color::Black).bg(ACCENT).bold()
    } else {
        Style::new().fg(ACCENT).bold()
    };
    let base_button = vec![
        Span::styled("[", Style::new().dim()),
        Span::styled(format!(" Base: {base} ⌄ "), button_style),
        Span::styled("]", Style::new().dim()),
    ];

    // Row 0: create a new branch off `base`; the section below checks out an
    // existing branch.
    let mut items: Vec<ListItem> = Vec::new();
    let typed = name.as_str().trim();
    let mut row0 = if typed.is_empty() {
        vec![
            Span::styled("+ ", Style::new().fg(Color::Green).bold()),
            Span::styled("type a name above → new branch off ", Style::new().dim()),
        ]
    } else {
        vec![
            Span::styled("+ ", Style::new().fg(Color::Green).bold()),
            Span::raw(format!("new branch '{typed}' off ")),
        ]
    };
    row0.extend(base_button);
    items.push(ListItem::new(Line::from(row0)));
    if !filtered.is_empty() {
        let header = if name.as_str().trim().is_empty() {
            "  or check out an existing branch:".to_string()
        } else {
            format!("  or check out a match ({}):", filtered.len())
        };
        items.push(ListItem::new(Line::styled(header, Style::new().dim())));
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
    // The section header is a non-selectable row, so shift the highlight past it
    // for any existing-branch selection. While the base button is focused, drop
    // the row highlight so only the button reads as selected.
    let highlight_row = if base_focus {
        None
    } else if selected == 0 {
        Some(0)
    } else {
        Some(selected + 1)
    };
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default();
    state.select(highlight_row);
    frame.render_stateful_widget(list, list_area, &mut state);

    // Reminder that the base is Tab-reachable, shown while the new-branch row is
    // in play (either editing the name or with the button focused).
    if selected == 0 {
        let hint = if base_focus {
            "Enter / Space: pick base branch  ·  Esc: back to name"
        } else {
            "⇥ Tab: focus the base button ⌄  ·  Enter: create"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(hint, Style::new().dim())),
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
    frame.render_widget(panel("branch off of"), popup);
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

fn draw_creating(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    lines: &[String],
    done: bool,
    input: &str,
    kill_armed: bool,
) {
    let input_rows = u16::from(!done);
    let height =
        (lines.len() as u16 + 2 + input_rows).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = centered(area, 76, height);
    frame.render_widget(Clear, popup);
    let title = if done {
        // The final ✓/✗ line (pushed right before "press Enter to continue")
        // says which; the title mirrors it so it's visible even scrolled off.
        let failed = lines.iter().any(|l| l.starts_with('✗'));
        if failed {
            format!("creating {branch} · failed")
        } else {
            format!("creating {branch} · ready")
        }
    } else {
        format!("creating {branch} · running…")
    };
    // Keep the tail visible when output exceeds the popup.
    let capacity = (height - 2 - input_rows) as usize;
    let skip = lines.len().saturating_sub(capacity);
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
    let para = Paragraph::new(text)
        .block(panel(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

/// Prompt for a one-off command to run in a worktree's directory.
fn draw_run_command(frame: &mut Frame, area: Rect, name: &str, input: &super::app::TextInput) {
    let popup = centered(area, 64, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(panel(format!("run in '{name}'")), popup);
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
    frame.render_widget(panel(format!("rename '{name}'")), popup);
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
    } else if line.starts_with("──") {
        Style::new().fg(ACCENT).dim()
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
    let key_span = Span::styled(
        format!("  {key:<12}"),
        Style::new().fg(ACCENT).bold(),
    );
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
    let block = panel("help");
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
/// message in the header, this can show a full multi-line git error and is
/// only dismissed by an explicit key press (see `App::error`, `on_key`).
fn draw_error_popup(frame: &mut Frame, area: Rect, msg: &str) {
    let width = 70.min(area.width);
    // Inner content width, accounting for the block's border and padding, used
    // to estimate how many visual lines the wrapped message will take.
    let inner_width = width.saturating_sub(4).max(1) as usize;
    let wrapped_lines: usize = msg
        .lines()
        .map(|line| line.chars().count().div_ceil(inner_width).max(1))
        .sum();
    // +2 for the border, +2 for the blank line and dismiss hint below the
    // message.
    let popup = modal_rect(area, wrapped_lines as u16, width, 4);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<Line> = msg.lines().map(Line::from).collect();
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Esc / Enter / any key to dismiss",
        Style::new().dim(),
    ));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::DANGER))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "error",
                Style::new().fg(theme::DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
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
    let block = panel(wizard_title(title, progress));
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
    let block = panel(wizard_title(&browser.dir.display().to_string(), progress))
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

/// The Settings tab: editable rows for the repo settings, the update-check
/// toggle, a live diff-theme colour sample under the theme row, the Worktrees
/// layout cycle, a live resolved-location preview, the running version, and a
/// check-for-updates row. Changes write to disk as soon as they are made. The
/// form keeps a fixed width inside the full-height panel so long paths don't
/// stretch it.
fn draw_settings_tab(
    frame: &mut Frame,
    area: Rect,
    editor: &ConfigEditor,
    update_available: Option<&Release>,
) -> Option<RowList> {
    let labels = [
        "worktree_dir     ",
        "open_command     ",
        "setup.copy       ",
        "setup.run        ",
        "auto_update_check",
        "diff_theme       ",
        "worktrees_layout ",
    ];
    let hints = [
        "sibling · inside · home · or a path ({repo} = repo name)",
        "commands the open key (o) runs; Enter edits the list, [ done ] saves. \
         {path} {name} {branch} {status}",
        "files copied into each new worktree, comma separated",
        "commands run in each new worktree, comma separated",
        "check GitHub for a newer wtm on start · Enter cycles · saved for all repos",
        "syntax colours in the diff pane · Enter cycles · saved for all repos",
        "Worktrees tab layout · three panels adds files + diff · Enter cycles · applies immediately",
    ];
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..FIELD_ROWS {
        let selected = row == editor.selected;
        let highlight = if selected {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        };
        let mut spans = vec![Span::styled(
            format!(" {} ", labels[row]),
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
            // The list row shows a one-line summary of its entries; Enter
            // opens the list editor drawn over the form below.
            _ if row == OPEN_COMMAND_ROW && !editor.fields.open_command.is_empty() => {
                spans.push(Span::styled(editor.open_command_summary(), highlight))
            }
            _ if editor.field(row).is_empty() => {
                spans.push(Span::styled("(default)".to_string(), highlight.dim()))
            }
            _ => spans.push(Span::styled(editor.field(row).to_string(), highlight)),
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(Span::styled(
            format!("   {}", hints[row]),
            Style::new().dim(),
        )));

        // Live colour sample sits under the theme row (before layout) so
        // cycling `diff_theme` shows the palette next to that setting.
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
            for sample in highlight::theme_preview_lines(theme_id) {
                // Indent under the arrow so the sample reads as part of the preview.
                let mut spans = vec![Span::raw("   ")];
                spans.extend(sample.spans);
                lines.push(Line::from(spans));
            }
        }
    }

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
    frame.render_widget(Paragraph::new(lines), form);
    // The list editor is modal over the form: it takes every key, so the form
    // reports no clickable rows while it is up.
    if let Some(list) = &editor.open_list {
        draw_open_command_list(frame, area, list);
        return None;
    }
    // The line layout is shared with `config_editor::row_at_line`, which
    // `on_click` uses to turn a clicked line back into a row.
    Some(RowList {
        inner: form,
        header: 0,
        offset: 0,
        len: FORM_LINES,
    })
}

/// The `open_command` list editor, floating over the Settings form: one row
/// per configured command, then an add row and a done row. The row being
/// typed shows the live buffer with a movable cursor.
fn draw_open_command_list(frame: &mut Frame, area: Rect, list: &OpenCommandEditor) {
    let rows = list.rows().clamp(3, 14) as u16;
    let popup = centered(area, 70, rows + 5);
    frame.render_widget(Clear, popup);
    let block = panel("open commands");
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
        let mut spans = vec![Span::styled(" ● ", style.fg(Color::Green))];
        match &list.input {
            Some(input) if list.editing_index == Some(index) => {
                push_cursor_spans(&mut spans, input.as_str(), input.cursor, style)
            }
            _ => spans.push(Span::styled(command.clone(), style)),
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
        "↑/↓ move · Enter edit · a add · d remove · [ done ] saves · Esc discard"
    };
    frame.render_widget(Paragraph::new(Line::from(hint.dim())), hint_area);
}

/// Commit dialog: a checklist of changed files (all ticked by default) above a
/// clearly labelled commit-message input. Focus moves between the two panes.
#[allow(clippy::too_many_arguments)]
fn draw_commit(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    files: &[StatusEntry],
    marked: &[bool],
    cursor: usize,
    input: &super::app::TextInput,
    focus: &CommitFocus,
) -> Option<RowList> {
    let list_rows = (files.len() as u16).clamp(1, 10);
    let popup = centered(area, 72, list_rows + 8);
    frame.render_widget(Clear, popup);
    frame.render_widget(panel(format!("commit · {name}")), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [files_area, label_area, prompt_area, hint_area] = Layout::vertical([
        Constraint::Length(list_rows + 1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let files_focused = *focus == CommitFocus::Files;
    let items: Vec<ListItem> = files
        .iter()
        .take(10)
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
    // Only the first 10 files are rendered, so clicks map onto that window.
    let list_hit = RowList {
        inner: files_area,
        header: 0,
        offset: 0,
        len: files.len().min(10),
    };

    // A label makes it obvious the prompt below is the commit message.
    let label_style = if files_focused {
        Style::new().dim()
    } else {
        Style::new().fg(ACCENT).bold()
    };
    frame.render_widget(
        Paragraph::new(Line::styled("Commit message:", label_style)),
        label_area,
    );
    frame.render_widget(
        Paragraph::new(prompt_line_at(input.as_str(), input.cursor)),
        prompt_area,
    );

    let selected_count = marked.iter().filter(|m| **m).count();
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!(
                "{selected_count}/{} file{} · Tab switches pane · Space toggles · Enter commits",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
            Style::new().dim(),
        )),
        hint_area,
    );
    Some(list_hit)
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
    Some(RowList {
        inner,
        header: 1,
        offset: state.offset(),
        len: app.stash_entries.len(),
    })
}

/// Top-of-main tab bar: the active tab in accent, the other dimmed, with a
/// reminder that Tab switches between them.
fn draw_tab_bar(frame: &mut Frame, area: Rect, app: &mut App) {
    let tab_span = |label: String, active: bool| {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::new().fg(Color::Black).bg(ACCENT).bold(),
            )
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
        let span = tab_span(format!("{} {}", tab.glyph(), tab.title()), app.tab == tab);
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

/// The Branches tab: a full-width table of local branches, with the inline
/// new-branch and confirm-delete popups floating on top. Returns the clickable
/// row list (suppressed while a popup is up).
fn draw_branches(frame: &mut Frame, area: Rect, app: &App) -> Option<RowList> {
    let title = if app.branches_loading() && !app.branches.is_empty() {
        "branches · refreshing…".to_string()
    } else {
        "branches".to_string()
    };
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
    let rows: Vec<Row> = app
        .branches
        .iter()
        .map(|b| {
            let name = Span::styled(b.name.clone(), Style::new().bold());
            let checkout = match (&b.checked_out_path, &b.remote) {
                (Some(p), _) => Span::styled(format!("● {p}"), Style::new().fg(theme::SUCCESS)),
                (None, Some(remote)) => {
                    Span::styled(format!("☁ {remote}"), Style::new().fg(theme::INFO).dim())
                }
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
            Constraint::Length(24),
            Constraint::Length(14),
            Constraint::Length(28),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["BRANCH", "CHECKED OUT", "UPSTREAM", "FLAGS", "LAST COMMIT"])
            .style(Style::new().dim().bold()),
    )
    .block(block)
    .row_highlight_style(Style::new().bg(SELECTION_BG).bold())
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
    let mut state = TableState::default().with_selected(Some(app.branch_selected));
    frame.render_stateful_widget(table, area, &mut state);

    // The create/rename/delete prompts are modals now, drawn over this list.
    Some(RowList {
        inner,
        header: 1,
        offset: state.offset(),
        len: app.branches.len(),
    })
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
    let block = panel(format!("switch '{name}' to branch"));
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

/// Renders one `git log --graph` art prefix, translating git's ASCII (`* | / \`)
/// into box-drawing characters and coloring each column by its lane. Empty in
/// flat mode, where rows carry no art.
fn graph_spans(graph: &str) -> Vec<Span<'static>> {
    graph
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
            // git spaces lanes two columns apart, so halving the column index
            // gives each lane one stable color.
            let color = GRAPH_COLORS[(col / 2) % GRAPH_COLORS.len()];
            Span::styled(ch.to_string(), Style::new().fg(color))
        })
        .collect()
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
/// `hash_width` abbreviates the full hashes the branch view stores.
fn commit_spans(e: &crate::git::LogEntry, hash_width: usize) -> Vec<Span<'static>> {
    let short = &e.hash[..e.hash.len().min(hash_width)];
    let mut spans = vec![Span::styled(
        format!("{short} "),
        Style::new().fg(Color::Yellow),
    )];
    spans.extend(ref_spans(&e.refs));
    spans.push(Span::raw(format!("{}  ", e.subject)));
    spans.push(Span::styled(
        format!("{} · {}", e.author, e.date),
        Style::new().dim(),
    ));
    spans
}

/// Scrollable commit log, styled like the diff view. In tree mode rows carry
/// graph art and some hold art alone; in flat mode every row is a commit.
fn draw_log(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    rows: &[GraphLine],
    selected: usize,
    mode: LogMode,
) -> Option<RowList> {
    let block = panel(format!("log · {name} · {}", mode.label()));
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let mut spans = graph_spans(&row.graph);
            if let Some(e) = &row.entry {
                spans.extend(commit_spans(e, usize::MAX));
            }
            ListItem::new(Line::from(spans))
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
                highlight::diff_lines(path, content)
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
fn draw_worktree_commits(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    rows: &[GraphLine],
    selected: usize,
    mode: LogMode,
    focused: bool,
) -> Option<RowList> {
    let block = focus_panel(format!("commits · {branch} · {}", mode.label()), focused);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let mut spans = graph_spans(&row.graph);
            if let Some(e) = &row.entry {
                spans.extend(commit_spans(e, 9));
            }
            ListItem::new(Line::from(spans))
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
fn draw_branch_commits(
    frame: &mut Frame,
    area: Rect,
    branch: &str,
    rows: &[GraphLine],
    marked: &[bool],
    selected: usize,
    mode: LogMode,
) -> Option<RowList> {
    let block = panel(format!("commits · {branch} · {}", mode.label()));
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no commits".dim())).block(block),
            area,
        );
        return None;
    }
    let inner = block.inner(area);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let Some(e) = &row.entry else {
                // An art-only row has no checkbox; pad past that column so its
                // graph still lines up with the commits above and below.
                let mut spans = vec![Span::raw("    ")];
                spans.extend(graph_spans(&row.graph));
                return ListItem::new(Line::from(spans));
            };
            let checked = marked.get(i).copied().unwrap_or(false);
            let mut spans = vec![if checked {
                Span::styled("[x] ", Style::new().fg(theme::SUCCESS))
            } else {
                Span::styled("[ ] ", Style::new().dim())
            }];
            spans.extend(graph_spans(&row.graph));
            // Full hashes are stored for cherry-pick; show an abbreviated form.
            spans.extend(commit_spans(e, 9));
            ListItem::new(Line::from(spans))
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
                Paragraph::new(lines).block(panel("cherry-pick mode")),
                popup,
            );
            None
        }
        // Worktree picker.
        None => {
            let rows = targets.len().clamp(1, 12) as u16;
            let popup = centered(area, 60, rows + 5);
            frame.render_widget(Clear, popup);
            let block = panel(format!("cherry-pick {n} {plural} from '{source_branch}'"));
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
    let rows = targets.len().clamp(1, 12) as u16;
    let popup = centered(area, 60, rows + 5);
    frame.render_widget(Clear, popup);
    let block = panel(format!("merge '{source_branch}' into worktree"));
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
        Paragraph::new(Line::from("↑/↓ pick · Enter merge · Esc cancel".dim())),
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
    let block = panel(format!("move changes from '{from}' into…"));
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
    commands: &[String],
    selected: usize,
) -> Option<RowList> {
    // Expanded commands carry absolute paths, so this picker is wider than the
    // worktree pickers around it.
    let rows = commands.len().clamp(1, 12) as u16;
    let popup = centered(area, 80, rows + 5);
    frame.render_widget(Clear, popup);
    let block = panel(format!("open '{name}' with…"));
    frame.render_widget(&block, popup);
    let inner = block.inner(popup);
    let [head_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from("this is what will run".dim())),
        head_area,
    );
    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(Color::Green)),
                Span::raw(expand_open_command(cmd, vars)),
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
    let block = panel(format!("{verb} {label} into…"));
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

/// Short label and color for a hunk's chosen resolution action.
fn action_label(action: Option<&ResolutionAction>) -> (&'static str, Color) {
    match action {
        None => ("… pick a side", BORDER),
        Some(ResolutionAction::KeepOurs) => ("OURS", theme::SUCCESS),
        Some(ResolutionAction::KeepTheirs) => ("THEIRS", Color::Blue),
        Some(ResolutionAction::KeepBoth) => ("BOTH", Color::Cyan),
        Some(ResolutionAction::KeepBothReversed) => ("BOTH (theirs first)", Color::Cyan),
        Some(ResolutionAction::Manual(_)) => ("MANUAL", theme::WARNING),
    }
}

/// One side of a hunk, each line prefixed with `marker` and colored, capped so
/// a huge hunk can't blow out the pane. An empty side is called out explicitly.
fn push_side(lines: &mut Vec<Line<'static>>, marker: &str, text: &str, color: Color) {
    let body: Vec<&str> = text.lines().collect();
    if body.is_empty() {
        lines.push(Line::styled(
            format!("  {marker} (nothing on this side)"),
            Style::new().fg(color).dim(),
        ));
        return;
    }
    const MAX: usize = 200;
    for l in body.iter().take(MAX) {
        lines.push(Line::styled(
            format!("  {marker} {l}"),
            Style::new().fg(color),
        ));
    }
    if body.len() > MAX {
        lines.push(Line::styled(
            format!("  {marker} … {} more line(s)", body.len() - MAX),
            Style::new().fg(color).dim(),
        ));
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
        ResolveKind::CherryPick => "the cherry-pick",
        ResolveKind::StashPop { .. } => "the stash",
    }
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
) -> Option<RowList> {
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(20)]).areas(area);

    // Left: conflicted files, each with a resolved/unresolved marker.
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let done = resolved.get(i).copied().unwrap_or(false);
            let mark = if done {
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
    let block = panel(format!("conflicts · {target}"));
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

    // Right: a resolved note, or the file's hunks.
    let Some(rf) = current else {
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                "  ✓ resolved — no conflicts remain in this file",
                Style::new().fg(theme::SUCCESS),
            ),
            Line::from(""),
            Line::styled(
                format!("  incoming from {} · {source_label}", incoming_source(kind)),
                Style::new().dim(),
            ),
            Line::styled(
                "  press c to complete once every file is done",
                Style::new().dim(),
            ),
        ])
        .block(panel(format!("resolve · {path}")));
        frame.render_widget(para, detail_area);
        return list_hit;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Spell out which side is which: OURS is what is already in this worktree
    // (the local/current branch), THEIRS is what is being pulled in.
    lines.push(Line::from(vec![
        Span::styled("‹ OURS ", Style::new().fg(theme::SUCCESS).bold()),
        Span::styled(
            format!("(current · {})", rf.file.ours_label),
            Style::new().fg(theme::SUCCESS),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("› THEIRS ", Style::new().fg(Color::Blue).bold()),
        Span::styled(
            format!(
                "(incoming from {} · {})",
                incoming_source(kind),
                rf.file.theirs_label
            ),
            Style::new().fg(Color::Blue),
        ),
    ]));
    lines.push(Line::from(""));

    let mut hunk_i = 0usize;
    // Line offset of the current hunk's header, used to keep it in view.
    let mut current_line = 0usize;
    for seg in &rf.file.segments {
        match seg {
            ConflictSegment::Plain(text) => {
                for l in context_lines(text) {
                    lines.push(Line::styled(format!("  {l}"), Style::new().dim()));
                }
            }
            ConflictSegment::Hunk { ours, theirs, .. } => {
                let is_cur = hunk_i == rf.hunk;
                if is_cur {
                    current_line = lines.len();
                }
                let (label, color) = action_label(rf.actions.get(hunk_i).and_then(|a| a.as_ref()));
                let marker = if is_cur { "◆" } else { "◇" };
                let hstyle = if is_cur {
                    Style::new().bg(SELECTION_BG).bold()
                } else {
                    Style::new().bold()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} hunk {} ", hunk_i + 1), hstyle.fg(ACCENT)),
                    Span::styled(format!("[{label}]"), Style::new().fg(color).bold()),
                ]));
                push_side(&mut lines, "‹", ours, theme::SUCCESS);
                lines.push(Line::styled("  ─────", Style::new().dim()));
                push_side(&mut lines, "›", theirs, Color::Blue);
                lines.push(Line::from(""));
                hunk_i += 1;
            }
        }
    }

    // Scroll so the current hunk's header sits near the top of the pane.
    let scroll = current_line.saturating_sub(1) as u16;
    let total = lines.len();
    let para = Paragraph::new(lines)
        .block(panel(format!("resolve · {path}")))
        .scroll((scroll, 0));
    frame.render_widget(para, detail_area);
    let mut sb = ScrollbarState::new(total.saturating_sub(detail_area.height as usize))
        .position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::new().fg(BORDER))
            .thumb_style(Style::new().fg(ACCENT)),
        detail_area,
        &mut sb,
    );
    // The manual hunk editor floats over the resolver as a modal (`draw_modal`).
    list_hit
}

/// Floating multi-line editor for hand-editing one hunk's resolved text, with a
/// visible block cursor. Saved with Ctrl+S, discarded with Esc.
fn draw_hunk_editor(frame: &mut Frame, area: Rect, hunk: usize, editor: &super::app::HunkEditor) {
    // Clamp bounds carefully: on a tiny terminal the available height can fall
    // below the preferred minimum, and `clamp` panics when min > max.
    let max_h = area.height.saturating_sub(2).max(3);
    let min_h = 6.min(max_h);
    let height = (editor.lines.len() as u16 + 4).clamp(min_h, max_h);
    let popup = centered(area, area.width.saturating_sub(8).min(90), height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        panel(format!("edit hunk {} · Ctrl+S save · Esc cancel", hunk + 1)),
        popup,
    );
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (r, text) in editor.lines.iter().enumerate() {
        if r == editor.row {
            // Split the cursor line so the character under the cursor is shown
            // inverted, giving a visible caret (or a trailing block at line end).
            let chars: Vec<char> = text.chars().collect();
            let mut spans = Vec::new();
            spans.push(Span::raw(
                chars[..editor.col.min(chars.len())]
                    .iter()
                    .collect::<String>(),
            ));
            let cursor_style = Style::new().bg(ACCENT).fg(Color::Black);
            if editor.col < chars.len() {
                spans.push(Span::styled(chars[editor.col].to_string(), cursor_style));
                spans.push(Span::raw(
                    chars[editor.col + 1..].iter().collect::<String>(),
                ));
            } else {
                spans.push(Span::styled(" ", cursor_style));
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::raw(text.clone()));
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
        .block(panel("please wait"));
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
            let block = panel(title.clone());
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
            frame.render_widget(Paragraph::new(lines).block(panel(title.clone())), popup);
            None
        }
        Modal::HunkEditor(editor) => {
            // The panel title numbers the hunk being edited, read from the
            // resolver screen underneath.
            let hunk = match &app.view {
                View::ConflictResolver {
                    current: Some(rf), ..
                } => rf.hunk,
                _ => 0,
            };
            draw_hunk_editor(frame, area, hunk, editor);
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
    const HUNK: &[Binding] = &[
        hint("type", "edit result"),
        hint("Ctrl+S", "save"),
        hint("Esc", "cancel"),
    ];
    match modal {
        Modal::Confirm { options, .. } if options.len() > 1 => CONFIRM_MULTI,
        Modal::Confirm { .. } => CONFIRM_SINGLE,
        Modal::Prompt { .. } => PROMPT,
        Modal::HunkEditor(_) => HUNK,
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
        CHECK_LINE, LAYOUT_LINE, LAYOUT_ROW, PREVIEW_LINE, THEME_PREVIEW_LABEL_LINE,
        THEME_PREVIEW_LINE, THEME_ROW, UPDATE_ROW, VERSION_LINE, line_of_row,
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
    /// art-only connector rows, and decorates the refs.
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
            draw_log(frame, area, "main", &rows, 0, LogMode::Tree);
        });
        assert!(out[0].contains("log · main · tree"), "{out:#?}");
        // git's `*` and `|` become `●` and `│`; the `\` becomes `╲`.
        assert!(
            out[1].contains("● 1a2b3c4 (HEAD -> main) merge feature"),
            "{out:#?}"
        );
        assert!(out[2].contains("│╲"), "{out:#?}");
        assert!(out[3].contains("│ ● 5d6e7f8 add tests"), "{out:#?}");
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
             s stash  p pull  ⇧P push  l log  d delete  ? help  q quit"
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
            draw_log(frame, area, "main", &rows, 0, LogMode::Flat);
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
            draw_branch_commits(frame, area, "main", &rows, &[true, false], 0, LogMode::Tree);
        });
        assert!(out[0].contains("commits · main · tree"), "{out:#?}");
        // A marked commit, its art, then the hash abbreviated to 9 chars.
        assert!(out[1].contains("[x] ● 1a2b3c4d5 merge feature"), "{out:#?}");
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
            draw_log(frame, area, "main", &[], 0, LogMode::Tree);
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
        }
    }

    /// The line offsets `config_editor::row_at_line` decodes clicks with must
    /// match what is actually drawn, or clicking a row would select another.
    #[test]
    fn settings_tab_draws_rows_where_the_click_decoder_expects_them() {
        let editor = settings_editor("false");
        let out = render(90, 30, |frame, area| {
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
            line(LAYOUT_LINE).contains("default: two panels"),
            "{:?}",
            line(LAYOUT_LINE)
        );
        // Theme preview belongs under the theme row, before layout.
        assert!(
            THEME_PREVIEW_LABEL_LINE > line_of_row(THEME_ROW),
            "theme preview must follow the theme row"
        );
        assert!(
            THEME_PREVIEW_LABEL_LINE < LAYOUT_LINE,
            "theme preview must sit above the layout row"
        );
        assert!(
            line(THEME_PREVIEW_LABEL_LINE).contains("diff colours look like"),
            "{:?}",
            line(THEME_PREVIEW_LABEL_LINE)
        );
        assert!(
            line(THEME_PREVIEW_LABEL_LINE).contains("Eighties"),
            "default theme label should appear: {:?}",
            line(THEME_PREVIEW_LABEL_LINE)
        );
        assert!(
            line(THEME_PREVIEW_LINE).contains("@@"),
            "sample hunk header missing: {:?}",
            line(THEME_PREVIEW_LINE)
        );
        assert!(
            line(THEME_PREVIEW_LINE + 1).contains("fn greet"),
            "sample removal missing: {:?}",
            line(THEME_PREVIEW_LINE + 1)
        );
        assert!(
            line(THEME_PREVIEW_LINE + 2).contains("fn greet"),
            "sample addition missing: {:?}",
            line(THEME_PREVIEW_LINE + 2)
        );
        assert!(line(PREVIEW_LINE).contains("new worktrees go in"));
        assert!(line(VERSION_LINE).contains(CURRENT_VERSION));
        assert!(line(CHECK_LINE).contains("check for updates"));
    }

    #[test]
    fn settings_tab_theme_preview_follows_the_selected_theme() {
        let mut editor = settings_editor("");
        editor.fields.diff_theme = "ocean".to_string();
        let out = render(90, 30, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let label = &out[2 + THEME_PREVIEW_LABEL_LINE];
        assert!(label.contains("Ocean"), "{label}");
        assert!(
            out[2 + THEME_PREVIEW_LINE + 1].contains("fn greet"),
            "{out:#?}"
        );
    }

    #[test]
    fn settings_tab_shows_the_version_and_any_update() {
        let editor = settings_editor("");
        // With nothing newer found, the version line says so.
        let out = render(90, 30, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let version_line = &out[2 + VERSION_LINE];
        assert!(version_line.contains(CURRENT_VERSION), "{version_line}");
        assert!(version_line.contains("up to date"), "{version_line}");
        // The unset toggle spells out which default it inherits.
        assert!(out[2 + UPDATE_ROW * 2].contains("default"), "{out:#?}");

        // A found release is called out instead.
        let release = Release {
            tag: "v9.9.9".to_string(),
            version: "9.9.9".to_string(),
            url: String::new(),
        };
        let out = render(90, 30, |frame, area| {
            draw_settings_tab(frame, area, &editor, Some(&release));
        });
        let version_line = &out[2 + VERSION_LINE];
        assert!(version_line.contains("9.9.9 available"), "{version_line}");
    }

    /// The list editor draws every command, the add row, and the done row, and
    /// covers the form (which reports no clickable rows while it is up).
    #[test]
    fn settings_tab_draws_the_open_command_list_editor() {
        let mut editor = settings_editor("");
        editor.selected = OPEN_COMMAND_ROW;
        editor.fields.open_command = vec!["cursor {path}".to_string(), "open {path}".to_string()];
        let mut list = OpenCommandEditor::new(editor.fields.open_command.clone());
        list.selected = 1;
        editor.open_list = Some(list);
        let hit = std::cell::Cell::new(true);
        let out = render(90, 30, |frame, area| {
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
        editor.fields.open_command = vec!["cursor {path}".to_string(), "open {path}".to_string()];
        let out = render(90, 30, |frame, area| {
            draw_settings_tab(frame, area, &editor, None);
        });
        let row = &out[2 + OPEN_COMMAND_ROW * 2];
        assert!(row.contains("2 commands"), "{row}");
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
                    "cursor {path}".to_string(),
                    "open {path} # {branch}".to_string(),
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
                    0,
                    LogMode::Flat,
                    true,
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
                    0,
                    LogMode::Flat,
                    false,
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
}
