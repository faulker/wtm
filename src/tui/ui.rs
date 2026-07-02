//! TUI rendering: worktree list, diff viewer, and dialogs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::app::{App, View};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    match &app.view {
        View::Diff {
            name,
            content,
            scroll,
        } => draw_diff(frame, main, name, content, *scroll),
        _ => draw_list(frame, main, app),
    }
    draw_footer(frame, footer, app);

    // Overlays on top of the list.
    match &app.view {
        View::Create { input } => draw_create_dialog(frame, main, input),
        View::Creating { branch, lines, .. } => draw_creating(frame, main, branch, lines),
        View::ConfirmDelete { name, dirty } => draw_confirm_delete(frame, main, name, *dirty),
        View::Help => draw_help(frame, main),
        _ => {}
    }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .worktrees
        .iter()
        .map(|wt| {
            let mut spans = vec![Span::styled(
                format!("{}{}", wt.name, if wt.is_main { "*" } else { "" }),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            if wt.dirty > 0 {
                spans.push(Span::styled(
                    format!("  {} change(s)", wt.dirty),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                spans.push(Span::styled("  clean", Style::default().fg(Color::Green)));
            }
            if let Some(ab) = wt.ahead_behind {
                spans.push(Span::styled(
                    format!("  +{} -{}", ab.ahead, ab.behind),
                    Style::default().fg(Color::Cyan),
                ));
            }
            if wt.locked {
                spans.push(Span::styled("  locked", Style::default().fg(Color::Red)));
            }
            spans.push(Span::styled(
                format!("  {}", wt.path),
                Style::default().dim(),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(" worktrees — {} ", app.ctx.repo_root.display());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(frame: &mut Frame, area: Rect, name: &str, content: &str, scroll: u16) {
    let lines: Vec<Line> = if content.is_empty() {
        vec![Line::from("no uncommitted changes".dim())]
    } else {
        content.lines().map(diff_line).collect()
    };
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" diff: {name} ")),
        )
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

/// Colors one diff line by its prefix.
fn diff_line(line: &str) -> Line<'_> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        Style::default().add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with("diff --git") {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Magenta)
    } else {
        Style::default()
    };
    Line::from(Span::styled(line, style))
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = match (&app.message, &app.view) {
        (Some(msg), _) => msg.clone(),
        (None, View::List) => {
            "↑/↓ select  Enter diff  n new  d delete  r refresh  ? help  q quit".to_string()
        }
        (None, View::Diff { .. }) => "↑/↓ scroll  PgUp/PgDn page  q back".to_string(),
        (None, View::Create { .. }) => "Enter create  Esc cancel".to_string(),
        (None, View::Creating { .. }) => "running setup…".to_string(),
        (None, View::ConfirmDelete { .. }) => "confirm deletion".to_string(),
        (None, View::Help) => "any key to close".to_string(),
    };
    frame.render_widget(Paragraph::new(text).dim(), area);
}

fn draw_create_dialog(frame: &mut Frame, area: Rect, input: &str) {
    let popup = centered(area, 60, 3);
    frame.render_widget(Clear, popup);
    let para = Paragraph::new(format!("{input}▏")).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" new worktree — branch name "),
    );
    frame.render_widget(para, popup);
}

fn draw_creating(frame: &mut Frame, area: Rect, branch: &str, lines: &[String]) {
    let height = (lines.len() as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    let popup = centered(area, 70, height);
    frame.render_widget(Clear, popup);
    // Keep the tail visible when output exceeds the popup.
    let visible = lines.len().saturating_sub((height - 2) as usize);
    let text: Vec<Line> = lines[visible..]
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" creating {branch} ")),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

fn draw_confirm_delete(frame: &mut Frame, area: Rect, name: &str, dirty: usize) {
    let popup = centered(area, 60, 4);
    frame.render_widget(Clear, popup);
    let lines = if dirty > 0 {
        vec![
            Line::from(Span::styled(
                format!("'{name}' has {dirty} uncommitted change(s)!"),
                Style::default().fg(Color::Red),
            )),
            Line::from("press f to force-delete (discards changes), Esc to cancel"),
        ]
    } else {
        vec![
            Line::from(format!("remove worktree '{name}'?")),
            Line::from("y to confirm, Esc to cancel"),
        ]
    };
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" delete "));
    frame.render_widget(para, popup);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 56, 12);
    frame.render_widget(Clear, popup);
    let text = vec![
        Line::from("↑/↓ or j/k   select worktree"),
        Line::from("Enter        view diff of uncommitted changes"),
        Line::from("n            create a new worktree"),
        Line::from("d            delete the selected worktree"),
        Line::from("r            refresh the list"),
        Line::from("q            quit"),
        Line::from(""),
        Line::from("setup steps for new worktrees come from .wtm.toml"),
        Line::from("in the repo root (files to copy, commands to run)."),
    ];
    let para = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" help "));
    frame.render_widget(para, popup);
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
