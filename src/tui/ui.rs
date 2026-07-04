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
    App, BranchMode, CommitFocus, DiffRow, IgnorePrompt, RowList, StashMode, View,
    filtered_branches,
};
use super::config_editor::{ConfigEditor, FIELD_ROWS, ROWS as CONFIG_ROWS};
use super::setup::{REVIEW_ROWS, SetupWizard, Step, location_preview};
use crate::config::{DEFAULT_LOCATION, LOCATION_PRESETS};
use crate::git::{LogEntry, StashEntry, StatusEntry};
use crate::ops::BranchListItem;

/// Single accent used for titles, keys, and selection markers.
const ACCENT: Color = Color::Cyan;
/// Border color for all panels.
const BORDER: Color = Color::DarkGray;
/// Background of the selected row in lists and tables.
const SELECTION_BG: Color = Color::DarkGray;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    // The full-screen view's clickable list, if any.
    let list_hit = match &app.view {
        View::Diff {
            name,
            files,
            marked,
            rows,
            selected,
            content,
            scroll,
            confirm_revert,
            ignore_prompt,
            ..
        } => draw_diff(
            frame,
            main,
            name,
            files,
            marked,
            rows,
            *selected,
            content,
            *scroll,
            *confirm_revert,
            ignore_prompt.as_ref(),
        ),
        View::Log {
            name,
            entries,
            scroll,
        } => {
            draw_log(frame, main, name, entries, *scroll);
            None
        }
        _ => None,
    };
    let list_hit = if matches!(app.view, View::Diff { .. } | View::Log { .. }) {
        list_hit
    } else {
        draw_list(frame, main, app)
    };
    draw_footer(frame, footer, app);

    // Overlays on top of the list. An overlay with its own selectable list
    // reports it here so clicks land on the overlay, not the list beneath it.
    let mut overlay_hit = None;
    match &app.view {
        View::Create {
            input,
            branches,
            selected,
        } => draw_create_dialog(
            frame,
            main,
            input,
            branches,
            *selected,
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
        View::ConfirmDelete {
            name,
            dirty,
            branch,
            delete_branch,
        } => draw_confirm_delete(frame, main, name, *dirty, branch.as_deref(), *delete_branch),
        View::Help => draw_help(frame, main),
        View::Setup(wizard) => draw_setup(frame, main, wizard),
        View::Config(editor) => draw_config(frame, main, editor),
        View::Commit {
            name,
            files,
            marked,
            cursor,
            input,
            focus,
        } => overlay_hit = draw_commit(frame, main, name, files, marked, *cursor, input, focus),
        View::Stash {
            name,
            entries,
            selected,
            mode,
        } => draw_stash(frame, main, name, entries, *selected, mode),
        View::Branch {
            branches,
            selected,
            mode,
        } => draw_branch(frame, main, branches, *selected, mode),
        View::Busy { label, .. } => draw_busy(frame, main, label),
        _ => {}
    }

    // Clicks go to the topmost selectable list: an overlay's own list when one
    // is up, otherwise the full-screen list for views that respond to clicks.
    // Other overlays cover the list, so clicks are disabled while they're up.
    app.row_list = match &app.view {
        View::List | View::Diff { .. } => list_hit,
        View::Commit { .. } => overlay_hit,
        _ => None,
    };
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

/// Top bar: app badge and repo path on the left; the worktree count on the
/// right, or the transient status/error message when one is present.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let count = app.worktrees.len();
    let left = Line::from(vec![
        Span::styled(" wtm ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
        Span::styled(app.ctx.repo_root.display().to_string(), Style::new().bold()),
    ]);
    // The right slot is wide enough for the message (or count), and is drawn
    // right-aligned so it never overlaps the app badge.
    let right = match &app.message {
        Some(msg) => {
            let style = if msg.starts_with("error") {
                Style::new().fg(Color::Red).bold()
            } else {
                Style::new().fg(Color::Yellow).bold()
            };
            Line::styled(format!("{msg} "), style)
        }
        None => Line::styled(
            format!("({count} worktree{}) ", if count == 1 { "" } else { "s" }),
            Style::new().dim(),
        ),
    };
    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// Footer as key hints: the key in accent, its label dimmed.
fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {label}"), Style::new().dim()));
    }
    Line::from(spans)
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) -> Option<RowList> {
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
                    Style::new().fg(Color::Yellow),
                )
            } else {
                Span::styled("clean".to_string(), Style::new().fg(Color::Green))
            };
            let upstream = match wt.ahead_behind {
                Some(ab) => Span::styled(
                    format!("↑{} ↓{}", ab.ahead, ab.behind),
                    Style::new().fg(ACCENT),
                ),
                None => Span::styled("–".to_string(), Style::new().dim()),
            };
            let flags = if wt.locked {
                Span::styled("locked", Style::new().fg(Color::Red))
            } else {
                Span::raw("")
            };
            Row::new(vec![
                Cell::from(name),
                Cell::from(changes),
                Cell::from(upstream),
                Cell::from(flags),
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
    let block = panel("worktrees");
    let inner = block.inner(area);
    let table = Table::new(
        rows,
        [
            Constraint::Length(name_w),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
    )
    .header(Row::new(["NAME", "CHANGES", "UPSTREAM", "", "PATH"]).style(Style::new().dim().bold()))
    .block(block)
    .row_highlight_style(Style::new().bg(SELECTION_BG).bold())
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
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
    scroll: u16,
    confirm_revert: bool,
    ignore_prompt: Option<&IgnorePrompt>,
) -> Option<RowList> {
    if files.is_empty() {
        let para = Paragraph::new(Line::from("no uncommitted changes".dim()))
            .block(panel(format!("changes · {name}")));
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
            } => {
                let indent = "  ".repeat(*depth);
                let states: Vec<bool> = files
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.path.starts_with(prefix.as_str()))
                    .map(|(i, _)| marked.get(i).copied().unwrap_or(false))
                    .collect();
                let check = if states.iter().all(|s| *s) {
                    Span::styled("[x] ", Style::new().fg(Color::Green))
                } else if states.iter().any(|s| *s) {
                    Span::styled("[~] ", Style::new().fg(ACCENT))
                } else {
                    Span::styled("[ ] ", Style::new().dim())
                };
                ListItem::new(Line::from(vec![
                    check,
                    Span::raw(indent),
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
                    Span::styled("[x] ", Style::new().fg(Color::Green))
                } else {
                    Span::styled("[ ] ", Style::new().dim())
                };
                let code = files.get(*index).map(|f| f.code.trim()).unwrap_or("");
                let style = files.get(*index).map(|f| status_style(&f.code)).unwrap_or_default();
                ListItem::new(Line::from(vec![
                    check,
                    Span::raw(indent),
                    Span::styled(format!("{code:<3}"), style),
                    Span::raw(label.clone()),
                ]))
            }
        })
        .collect();
    let block = panel(format!("files · {name}"));
    let inner = block.inner(list_area);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
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
            let lines = if content.is_empty() {
                vec![Line::from("no textual diff (binary or empty)".dim())]
            } else {
                content.lines().map(diff_line).collect()
            };
            (format!("diff · {path}"), lines)
        }
    };
    let total = lines.len();
    let para = Paragraph::new(lines).block(panel(title)).scroll((scroll, 0));
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

    if confirm_revert {
        let label = current_diff_path(rows, files, selected);
        let label = if label.is_empty() {
            "discard changes?".to_string()
        } else {
            format!("discard all changes to '{label}'?")
        };
        draw_confirm_popup(
            frame,
            area,
            "revert file",
            &label,
            "y to discard · Esc to cancel",
        );
    }

    if let Some(prompt) = ignore_prompt {
        draw_ignore_prompt(frame, area, prompt);
    }

    // A confirm/ignore popup is modal, so suppress list clicks behind it.
    if confirm_revert || ignore_prompt.is_some() {
        None
    } else {
        Some(list_hit)
    }
}

/// Popup for adding the highlighted file to `.gitignore`: ignore just this file,
/// or a glob pattern that matches every file like it.
fn draw_ignore_prompt(frame: &mut Frame, area: Rect, prompt: &IgnorePrompt) {
    let popup = centered(area, 64, 7);
    frame.render_widget(Clear, popup);
    let option = |selected: bool, label: String| -> Line<'static> {
        let marker = if selected { "▌ ● " } else { "  ○ " };
        let style = if selected {
            Style::new().bg(SELECTION_BG).bold()
        } else {
            Style::new()
        };
        Line::from(vec![
            Span::styled(marker.to_string(), style.fg(ACCENT)),
            Span::styled(label, style),
        ])
    };
    let (exact, glob) = if prompt.is_folder {
        ("just this folder", "all folders like it")
    } else {
        ("just this file", "all files like it")
    };
    let lines = vec![
        Line::from("add to .gitignore:"),
        Line::from(""),
        option(prompt.selected == 0, format!("{exact}: {}", prompt.file)),
        option(prompt.selected == 1, format!("{glob}: {}", prompt.pattern)),
        Line::from(""),
        Line::from("↑/↓ choose · Enter confirm · Esc cancel".dim()),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel("ignore")), popup);
}

/// Colors one diff line by its prefix.
fn diff_line(line: &str) -> Line<'_> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        Style::new().add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::new().fg(ACCENT)
    } else if line.starts_with("diff --git") {
        Style::new().add_modifier(Modifier::BOLD).fg(Color::Magenta)
    } else {
        Style::new()
    };
    Line::from(Span::styled(line, style))
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    // The status message lives in the header now, so the key hints below stay
    // visible at all times.
    let hints: &[(&str, &str)] = match &app.view {
        View::List => &[
            ("↑/↓", "select"),
            ("Enter", "changes"),
            ("n", "new"),
            ("⇧C", "commit"),
            ("s", "stash"),
            ("p", "pull"),
            ("⇧P", "push"),
            ("f", "fetch"),
            ("b", "branch"),
            ("l", "log"),
            ("d", "delete"),
            ("?", "help"),
            ("q", "quit"),
        ],
        View::Diff {
            confirm_revert: true,
            ..
        } => &[("y", "discard changes"), ("Esc", "cancel")],
        View::Diff { .. } => &[
            ("↑/↓", "row"),
            ("⇧↑/⇧↓/⇧J/⇧K/wheel", "scroll diff"),
            ("Space", "mark file/folder"),
            ("⇧C", "commit"),
            ("s", "stash file"),
            ("⇧R", "revert file"),
            ("i", "ignore file/folder"),
            ("q", "back"),
        ],
        View::Log { .. } => &[
            ("↑/↓", "scroll"),
            ("PgUp/PgDn", "page"),
            ("g", "top"),
            ("q", "back"),
        ],
        View::Commit { focus, .. } => match focus {
            CommitFocus::Files => &[
                ("↑/↓", "file"),
                ("Space", "toggle"),
                ("a", "all/none"),
                ("Tab", "message"),
                ("Enter", "commit"),
                ("Esc", "cancel"),
            ],
            CommitFocus::Message => &[
                ("type", "commit message"),
                ("Tab", "pick files"),
                ("Enter", "commit"),
                ("Esc", "cancel"),
            ],
        },
        View::Stash { mode, .. } => match mode {
            StashMode::List => &[
                ("↑/↓", "select"),
                ("s", "stash"),
                ("p", "pop"),
                ("a", "apply"),
                ("x", "drop"),
                ("Esc", "close"),
            ],
            StashMode::Message(_) => &[
                ("type", "message (optional)"),
                ("Enter", "stash"),
                ("Esc", "back"),
            ],
            StashMode::ConfirmDrop => &[("y", "drop"), ("Esc", "cancel")],
        },
        View::Branch { mode, .. } => match mode {
            BranchMode::List => &[
                ("↑/↓", "select"),
                ("Enter", "check out in a worktree"),
                ("n", "new branch (no worktree)"),
                ("x", "delete"),
                ("Esc", "close"),
            ],
            BranchMode::Create(_) => &[
                ("type", "branch name"),
                ("Enter", "create"),
                ("Esc", "back"),
            ],
            BranchMode::ConfirmDelete => &[("y", "delete"), ("f", "force"), ("Esc", "cancel")],
        },
        View::Busy { .. } => &[("", "working…")],
        View::Create { .. } => &[
            ("type", "filter / name a branch"),
            ("↑/↓", "pick"),
            ("Enter", "create worktree"),
            ("Esc", "cancel"),
        ],
        View::Creating { done: false, .. } => &[
            ("type + Enter", "answer a prompt"),
            ("Ctrl+C ×2", "kill setup"),
        ],
        View::Creating { .. } => &[("Enter", "close")],
        View::ConfirmDelete { .. } => &[
            ("↑/↓", "choose"),
            ("Enter", "confirm"),
            ("f", "force"),
            ("Esc", "cancel"),
        ],
        View::Help => &[("any key", "close")],
        View::Config(editor) if editor.editing.is_some() => {
            &[("Enter", "save value"), ("Esc", "cancel edit")]
        }
        View::Config(_) => &[("↑/↓", "select"), ("Enter", "edit/save"), ("Esc", "cancel")],
        View::Setup(wizard) => match &wizard.step {
            Step::CloneAsk { .. } => &[("←/→", "choose"), ("Enter", "confirm"), ("Esc", "quit")],
            Step::ClonePath { .. } => &[
                ("type", "a path"),
                ("Tab", "browse"),
                ("Enter", "load"),
                ("Esc", "back"),
            ],
            Step::CloneBrowse { .. } => &[
                ("↑/↓", "select"),
                ("Enter", "open/pick"),
                ("Backspace", "up"),
                ("Esc", "back"),
            ],
            Step::Location { .. } => &[("↑/↓", "select"), ("Enter", "confirm"), ("Esc", "back")],
            Step::LocationCustom { .. } | Step::CopyFiles { .. } => {
                &[("Enter", "confirm"), ("Esc", "back")]
            }
            Step::RunCommands { .. } => &[
                ("Enter", "add command"),
                ("blank Enter", "finish"),
                ("Esc", "back"),
            ],
            Step::Review {
                editing: Some(_), ..
            } => &[("Enter", "save"), ("Esc", "cancel edit")],
            Step::Review { .. } => &[
                ("↑/↓", "select"),
                ("Enter", "edit/write"),
                ("Esc", "start over"),
            ],
        },
    };
    frame.render_widget(Paragraph::new(hint_line(hints)), area);
}

/// The typed input with a block cursor, styled as a prompt line.
fn prompt_line(input: &str) -> Line<'_> {
    Line::from(vec![
        Span::styled("❯ ", Style::new().fg(ACCENT).bold()),
        Span::raw(input),
        Span::styled("▏", Style::new().fg(ACCENT)),
    ])
}

fn draw_create_dialog(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    branches: &[String],
    selected: usize,
    base: Option<&str>,
) {
    let filtered = filtered_branches(branches, input);
    let list_rows = filtered.len().min(8) as u16;
    let popup = centered(area, 64, 6 + list_rows);
    frame.render_widget(Clear, popup);
    frame.render_widget(panel("new worktree"), popup);
    let inner = popup.inner(ratatui::layout::Margin::new(2, 1));
    let [input_area, list_area, hint_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(list_rows + 1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(prompt_line(input)), input_area);

    // Row 0: create a new branch from the input; then matching existing ones.
    let mut items: Vec<ListItem> = Vec::new();
    items.push(ListItem::new(Line::from(vec![
        Span::styled("+ ", Style::new().fg(Color::Green).bold()),
        if input.trim().is_empty() {
            Span::styled("type a name: new branch + worktree", Style::new().dim())
        } else {
            Span::raw(format!("new branch + worktree '{}'", input.trim()))
        },
    ])));
    for branch in &filtered {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("⎇ ", Style::new().fg(ACCENT)),
            Span::raw((*branch).clone()),
            Span::styled("  existing branch → worktree", Style::new().dim()),
        ])));
    }
    let list = List::new(items)
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);

    if let Some(base) = base {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("location: {base}"),
                Style::new().dim(),
            )),
            hint_area,
        );
    }
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
        format!("creating {branch} · finished")
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
                Style::new().fg(Color::Red).bold(),
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

/// Styles one line of setup output: step results and errors stand out,
/// echoed user input shows its prompt, plain command output stays dim.
fn output_line(line: &str) -> Line<'_> {
    let style = if line.starts_with("[ok]") {
        Style::new().fg(Color::Green)
    } else if line.starts_with("[FAILED]") || line.starts_with("error") {
        Style::new().fg(Color::Red)
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

fn draw_confirm_delete(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    dirty: usize,
    branch: Option<&str>,
    delete_branch: bool,
) {
    let extra = u16::from(dirty > 0);
    let popup = centered(area, 64, 7 + extra);
    frame.render_widget(Clear, popup);
    let mut lines = vec![Line::from(vec![
        Span::raw("remove worktree "),
        Span::styled(format!("'{name}'"), Style::new().bold()),
        Span::raw("?"),
    ])];
    if dirty > 0 {
        lines.push(Line::styled(
            format!("⚠ {dirty} uncommitted change(s) will be lost — press f to force"),
            Style::new().fg(Color::Red),
        ));
    }
    lines.push(Line::from(""));
    let option = |selected: bool, label: String| -> Line<'static> {
        let marker = if selected { "▌ ● " } else { "  ○ " };
        let style = if selected {
            Style::new().bg(SELECTION_BG).bold()
        } else {
            Style::new()
        };
        Line::from(vec![
            Span::styled(marker.to_string(), style.fg(ACCENT)),
            Span::styled(label, style),
        ])
    };
    match branch {
        Some(b) => {
            lines.push(option(
                !delete_branch,
                format!("remove folder only (keep branch '{b}')"),
            ));
            lines.push(option(
                delete_branch,
                format!("remove folder and delete branch '{b}'"),
            ));
        }
        None => lines.push(option(true, "remove the worktree folder".to_string())),
    }
    let para = Paragraph::new(lines).block(panel("delete"));
    frame.render_widget(para, popup);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 64, 23);
    frame.render_widget(Clear, popup);
    let key = |k: &str, label: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), Style::new().fg(ACCENT).bold()),
            Span::raw(label.to_string()),
        ])
    };
    let text = vec![
        key("↑/↓ or j/k", "select worktree"),
        key("Enter", "browse changes per file (diff, stash, revert)"),
        key("n", "new worktree (new or existing branch)"),
        key("⇧C", "commit (pick files, all selected by default)"),
        key("s", "stash manager (stash/pop/apply/drop)"),
        key("p", "pull the worktree (fast-forward only)"),
        key("⇧P", "push the worktree"),
        key("f", "fetch all remotes"),
        key("b", "branch browser (branch-only create/delete/checkout)"),
        key("l", "commit log of the worktree"),
        key("d", "delete worktree (folder, or folder + branch)"),
        key("c", "edit this repo's settings"),
        key("r", "refresh the list"),
        key("q / Ctrl+C", "quit"),
        Line::from(""),
        Line::from("in the changes view: files are grouped into a folder tree.".dim()),
        Line::from("Space marks a file (or a whole folder) for commit,".dim()),
        Line::from("s stashes it, ⇧R reverts it, ⇧C commits the marked files,".dim()),
        Line::from("i adds the file or folder (or a glob) to .gitignore.".dim()),
        Line::from(""),
        Line::from("worktree location and setup steps come from .wtm.toml.".dim()),
    ];
    let para = Paragraph::new(text).block(panel("help"));
    frame.render_widget(para, popup);
}

/// Renders the current step of the first-run setup wizard.
fn draw_setup(frame: &mut Frame, area: Rect, wizard: &SetupWizard) {
    match &wizard.step {
        Step::CloneAsk { yes } => draw_clone_ask(frame, area, *yes),
        Step::ClonePath { input } => draw_clone_path(frame, area, input),
        Step::CloneBrowse { browser, .. } => draw_browser(frame, area, browser),
        Step::Location { selected } => draw_location(frame, area, wizard, *selected),
        Step::LocationCustom { input } => draw_wizard_input(
            frame,
            area,
            "worktree location · path",
            input,
            "absolute, ~/..., or relative to the repo; {repo} = repo name",
        ),
        Step::CopyFiles { input } => draw_wizard_input(
            frame,
            area,
            "files to copy into new worktrees",
            input,
            "comma separated, e.g. .env, .env.local (blank for none)",
        ),
        Step::RunCommands { commands, input } => draw_run_commands(frame, area, commands, input),
        Step::Review { selected, editing } => {
            draw_review(frame, area, wizard, *selected, editing.as_deref())
        }
    }
}

fn draw_clone_ask(frame: &mut Frame, area: Rect, yes: bool) {
    let popup = centered(area, 60, 5);
    frame.render_widget(Clear, popup);
    let selected = Style::new().bg(SELECTION_BG).bold().fg(ACCENT);
    let plain = Style::new();
    let lines = vec![
        Line::from("This repo isn't set up for wtm yet."),
        Line::from("Clone settings from another repo?"),
        Line::from(vec![
            Span::styled(" yes ", if yes { selected } else { plain }),
            Span::raw("   "),
            Span::styled(" no ", if yes { plain } else { selected }),
        ]),
    ];
    let para = Paragraph::new(lines).block(panel("wtm setup"));
    frame.render_widget(para, popup);
}

fn draw_clone_path(frame: &mut Frame, area: Rect, input: &str) {
    let popup = centered(area, 70, 5);
    frame.render_widget(Clear, popup);
    let lines = vec![
        prompt_line(input),
        Line::from("path to a repo or a .wtm.toml file".dim()),
        Line::from("Tab opens a file browser".dim()),
    ];
    let para = Paragraph::new(lines).block(panel("clone settings from"));
    frame.render_widget(para, popup);
}

fn draw_browser(frame: &mut Frame, area: Rect, browser: &super::setup::FileBrowser) {
    let height = (browser.entries.len() as u16 + 2).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = centered(area, 70, height);
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
    let list = List::new(items)
        .block(panel(browser.dir.display().to_string()))
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(browser.selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

fn draw_location(frame: &mut Frame, area: Rect, wizard: &SetupWizard, selected: usize) {
    let popup = centered(area, 70, LOCATION_PRESETS.len() as u16 + 3);
    frame.render_widget(Clear, popup);
    let mut items: Vec<ListItem> = LOCATION_PRESETS
        .iter()
        .map(|(name, label)| {
            let preview = location_preview(name, &wizard.repo_root);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{label}: "), Style::new().bold()),
                Span::styled(preview, Style::new().dim()),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(Span::styled(
        "somewhere else: type a path",
        Style::new().bold(),
    ))));
    let list = List::new(items)
        .block(panel("where should new worktrees go?"))
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, popup, &mut state);
}

/// A single-line wizard text input with a hint underneath.
fn draw_wizard_input(frame: &mut Frame, area: Rect, title: &str, input: &str, hint: &str) {
    let popup = centered(area, 70, 4);
    frame.render_widget(Clear, popup);
    let lines = vec![prompt_line(input), Line::from(hint.to_string().dim())];
    let para = Paragraph::new(lines).block(panel(title));
    frame.render_widget(para, popup);
}

fn draw_run_commands(frame: &mut Frame, area: Rect, commands: &[String], input: &str) {
    let height = (commands.len() as u16 + 4).clamp(4, area.height.saturating_sub(2).max(4));
    let popup = centered(area, 70, height);
    frame.render_widget(Clear, popup);
    let mut lines: Vec<Line> = commands
        .iter()
        .map(|cmd| Line::from(format!("  {cmd}")))
        .collect();
    lines.push(prompt_line(input));
    lines.push(Line::from(
        "one command per line, blank Enter to finish".dim(),
    ));
    let para = Paragraph::new(lines).block(panel("commands to run in each new worktree"));
    frame.render_widget(para, popup);
}

fn draw_review(
    frame: &mut Frame,
    area: Rect,
    wizard: &SetupWizard,
    selected: usize,
    editing: Option<&str>,
) {
    let popup = centered(area, 74, 8);
    frame.render_widget(Clear, popup);
    let value = |row: usize| -> String {
        match row {
            0 => wizard.draft.worktree_dir.clone(),
            1 => wizard.draft.copy.join(", "),
            _ => wizard.draft.run.join(", "),
        }
    };
    let labels = ["worktree_dir", "setup.copy  ", "setup.run   "];
    let mut lines: Vec<Line> = Vec::new();
    for (row, label) in labels.iter().enumerate() {
        let highlight = if row == selected {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        };
        let shown = match (row == selected, editing) {
            (true, Some(buf)) => format!("{buf}▏"),
            _ => value(row),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label} "), highlight.bold()),
            Span::styled(shown, highlight),
        ]));
    }
    lines.push(Line::from(""));
    let write_row = REVIEW_ROWS - 1;
    let write_style = if selected == write_row {
        Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
    } else {
        Style::new().bold()
    };
    lines.push(Line::from(Span::styled(
        " [ write .wtm.toml ] ",
        write_style,
    )));
    // A cloned absolute path usually points at the other repo's location.
    if wizard.draft.worktree_dir.starts_with('/') || wizard.draft.worktree_dir.starts_with('~') {
        lines.push(Line::from(
            "check that this worktree_dir makes sense for this repo".dim(),
        ));
    }
    let para = Paragraph::new(lines).block(panel("review settings"));
    frame.render_widget(para, popup);
}

/// The repo settings editor: editable rows for worktree_dir, setup.copy, and
/// setup.run, a live resolved-location preview, and a save row.
fn draw_config(frame: &mut Frame, area: Rect, editor: &ConfigEditor) {
    let popup = centered(area, 76, 11);
    frame.render_widget(Clear, popup);
    let labels = ["worktree_dir", "setup.copy  ", "setup.run   "];
    let hints = [
        "sibling · inside · home · or a path ({repo} = repo name)",
        "files copied into each new worktree, comma separated",
        "commands run in each new worktree, comma separated",
    ];
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..FIELD_ROWS {
        let selected = row == editor.selected;
        let highlight = if selected {
            Style::new().bg(SELECTION_BG)
        } else {
            Style::new()
        };
        let shown = match (selected, &editor.editing) {
            (true, Some(buf)) => format!("{buf}▏"),
            _ if editor.field(row).is_empty() => "(default)".to_string(),
            _ => editor.field(row).to_string(),
        };
        let value_style = if editor.field(row).is_empty() && editor.editing.is_none() {
            highlight.dim()
        } else {
            highlight
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", labels[row]), highlight.fg(ACCENT).bold()),
            Span::styled(shown, value_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("   {}", hints[row]),
            Style::new().dim(),
        )));
    }

    // Live preview of where worktrees will actually be created.
    let raw_dir = if editor.worktree_dir.trim().is_empty() {
        DEFAULT_LOCATION
    } else {
        editor.worktree_dir.trim()
    };
    let resolved = crate::config::resolve_worktree_dir(raw_dir, &editor.repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(needs HOME set)".to_string());
    lines.push(Line::from(Span::styled(
        format!(" → new worktrees go in {resolved}"),
        Style::new().fg(Color::Green),
    )));

    let save_row = CONFIG_ROWS - 1;
    let save_style = if editor.selected == save_row {
        Style::new().bg(SELECTION_BG).bold().fg(ACCENT)
    } else {
        Style::new().bold()
    };
    lines.push(Line::from(Span::styled(" [ save .wtm.toml ] ", save_style)));

    let para = Paragraph::new(lines).block(panel("edit settings"));
    frame.render_widget(para, popup);
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
    input: &str,
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
                Span::styled("[x] ", Style::new().fg(Color::Green))
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
    frame.render_widget(Paragraph::new(prompt_line(input)), prompt_area);

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
        Some('?') => Style::new().fg(Color::Yellow),
        Some(' ') | None => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Green),
    }
}

/// Stash manager: the entry list, with a message input or drop confirm on top.
fn draw_stash(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    entries: &[StashEntry],
    selected: usize,
    mode: &StashMode,
) {
    let rows = (entries.len() as u16).clamp(1, 12);
    let popup = centered(area, 74, rows + 3);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = if entries.is_empty() {
        vec![ListItem::new(Line::from("(no stashes)".dim()))]
    } else {
        entries
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("stash@{{{}}} ", e.index), Style::new().fg(ACCENT)),
                    Span::raw(e.message.clone()),
                ]))
            })
            .collect()
    };
    let list = List::new(items)
        .block(panel(format!("stash · {name}")))
        .highlight_style(Style::new().bg(SELECTION_BG).bold())
        .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, popup, &mut state);

    match mode {
        StashMode::Message(buf) => draw_input_popup(
            frame,
            area,
            "stash message (optional)",
            buf,
            "blank Enter stashes without a message",
        ),
        StashMode::ConfirmDrop => {
            let entry = entries.get(selected);
            let label = entry
                .map(|e| format!("drop stash@{{{}}}?", e.index))
                .unwrap_or_else(|| "drop stash?".to_string());
            draw_confirm_popup(
                frame,
                area,
                "drop stash",
                &label,
                "y to drop · Esc to cancel",
            );
        }
        StashMode::List => {}
    }
}

/// Branch browser: one row per local branch, with a create input or delete
/// confirm on top.
fn draw_branch(
    frame: &mut Frame,
    area: Rect,
    branches: &[BranchListItem],
    selected: usize,
    mode: &BranchMode,
) {
    let popup = centered(area, 84, area.height.saturating_sub(2).clamp(6, 22));
    frame.render_widget(Clear, popup);
    let rows: Vec<Row> = branches
        .iter()
        .map(|b| {
            let name = Span::styled(b.name.clone(), Style::new().bold());
            let checkout = match &b.checked_out_path {
                Some(p) => Span::styled(format!("● {p}"), Style::new().fg(Color::Green)),
                None => Span::styled("–".to_string(), Style::new().dim()),
            };
            let track = if b.upstream.is_some() {
                Span::styled(
                    format!("↑{} ↓{}", b.ahead, b.behind),
                    Style::new().fg(ACCENT),
                )
            } else {
                Span::styled("no upstream".to_string(), Style::new().dim())
            };
            let last = Span::styled(format!("{}  {}", b.date, b.subject), Style::new().dim());
            Row::new(vec![
                Cell::from(Line::from(name)),
                Cell::from(Line::from(checkout)),
                Cell::from(Line::from(track)),
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
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["BRANCH", "CHECKED OUT", "UPSTREAM", "LAST COMMIT"])
            .style(Style::new().dim().bold()),
    )
    .block(panel("branches"))
    .row_highlight_style(Style::new().bg(SELECTION_BG).bold())
    .highlight_symbol(Span::styled("▌ ", Style::new().fg(ACCENT)));
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, popup, &mut state);

    match mode {
        BranchMode::Create(buf) => draw_input_popup(
            frame,
            area,
            "new branch (no worktree)",
            buf,
            "branch only, from HEAD · Esc cancels",
        ),
        BranchMode::ConfirmDelete => {
            let label = branches
                .get(selected)
                .map(|b| format!("delete branch '{}'?", b.name))
                .unwrap_or_else(|| "delete branch?".to_string());
            draw_confirm_popup(
                frame,
                area,
                "delete branch",
                &label,
                "y to delete · f to force · Esc to cancel",
            );
        }
        BranchMode::List => {}
    }
}

/// Scrollable commit log, styled like the diff view.
fn draw_log(frame: &mut Frame, area: Rect, name: &str, entries: &[LogEntry], scroll: u16) {
    let lines: Vec<Line> = if entries.is_empty() {
        vec![Line::from("no commits".dim())]
    } else {
        entries
            .iter()
            .map(|e| {
                Line::from(vec![
                    Span::styled(format!("{} ", e.hash), Style::new().fg(Color::Yellow)),
                    Span::raw(format!("{}  ", e.subject)),
                    Span::styled(format!("{} · {}", e.author, e.date), Style::new().dim()),
                ])
            })
            .collect()
    };
    let total = lines.len();
    let para = Paragraph::new(lines)
        .block(panel(format!("log · {name}")))
        .scroll((scroll, 0));
    frame.render_widget(para, area);
    let mut sb_state =
        ScrollbarState::new(total.saturating_sub(area.height as usize)).position(scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::new().fg(BORDER))
            .thumb_style(Style::new().fg(ACCENT)),
        area,
        &mut sb_state,
    );
}

/// A small centered overlay showing that a background op is running.
fn draw_busy(frame: &mut Frame, area: Rect, label: &str) {
    let popup = centered(area, (label.len() as u16 + 6).min(area.width), 3);
    frame.render_widget(Clear, popup);
    let para = Paragraph::new(Line::styled(label, Style::new().fg(ACCENT).bold()))
        .block(panel("please wait"));
    frame.render_widget(para, popup);
}

/// A generic single-line text input overlay with a dim hint underneath.
fn draw_input_popup(frame: &mut Frame, area: Rect, title: &str, input: &str, hint: &str) {
    let popup = centered(area, 64, 4);
    frame.render_widget(Clear, popup);
    let lines = vec![prompt_line(input), Line::from(hint.to_string().dim())];
    frame.render_widget(Paragraph::new(lines).block(panel(title.to_string())), popup);
}

/// A generic confirmation overlay: a question and a dim hint of the keys.
fn draw_confirm_popup(frame: &mut Frame, area: Rect, title: &str, question: &str, hint: &str) {
    let popup = centered(area, 60, 4);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::styled(question.to_string(), Style::new().bold()),
        Line::from(hint.to_string().dim()),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel(title.to_string())), popup);
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
