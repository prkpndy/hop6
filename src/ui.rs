//! Rendering. This module is a *read-only* view over [`App`] — it never mutates state and
//! never performs I/O. The layout is three regions:
//!
//! ```text
//! ┌─ status bar ───────────────────────────────────────────────┐
//! ├─ roster ──┬─ chat log (active conversation or system) ──────┤
//! │  *system  │  me: hi                                          │
//! │  abc123…  │  abc123…: hey                                    │
//! ├───────────┴─ input ────────────────────────────────────────┤
//! │ > type a message or /command                                │
//! └────────────────────────────────────────────────────────────┘
//! ```

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    let [status_area, body_area, input_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    let [roster_area, chat_area] =
        Layout::horizontal([Constraint::Length(24), Constraint::Min(1)]).areas(body_area);

    render_status(frame, status_area, app);
    render_roster(frame, roster_area, app);
    render_chat(frame, chat_area, app);
    render_input(frame, input_area, app);
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let identity = app
        .own_onion
        .as_deref()
        .unwrap_or("(publishing onion service…)");
    let line = Line::from(vec![
        Span::styled("hop6", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::raw("  you: "),
        Span::styled(identity, Style::default().fg(Color::Cyan)),
        Span::raw("   "),
        Span::styled(app.status.clone(), Style::default().fg(Color::Yellow)),
    ]);
    let widget = Paragraph::new(line)
        .block(Block::default().borders(Borders::ALL).title(" status "));
    frame.render_widget(widget, area);
}

fn render_roster(frame: &mut Frame, area: Rect, app: &App) {
    let mut items: Vec<ListItem> = Vec::with_capacity(app.order.len() + 1);

    // The system pane is selectable via `active == None`.
    items.push(roster_item("*system", app.active.is_none(), true));

    for &id in &app.order {
        if let Some(p) = app.peers.get(&id) {
            let selected = app.active == Some(id);
            let label = if p.onion.is_empty() {
                format!("[{id}] (incoming)")
            } else {
                format!("[{id}] {}", crate::app::short_onion(&p.onion))
            };
            items.push(roster_item(&label, selected, p.connected));
        }
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" peers "));
    frame.render_widget(list, area);
}

fn roster_item(label: &str, selected: bool, online: bool) -> ListItem<'static> {
    let mut style = if online {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if selected {
        style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    let marker = if selected { "▶ " } else { "  " };
    ListItem::new(Line::from(Span::styled(format!("{marker}{label}"), style)))
}

fn render_chat(frame: &mut Frame, area: Rect, app: &App) {
    let (title, lines) = build_chat_lines(app);

    // Bottom-anchored view with PageUp/PageDown scroll. `area.height - 2` accounts for borders.
    let viewport = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let end = total.saturating_sub(app.scroll);
    let start = end.saturating_sub(viewport);
    let visible: Vec<Line> = lines[start..end].to_vec();

    let widget = Paragraph::new(visible)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn build_chat_lines(app: &App) -> (String, Vec<Line<'static>>) {
    match app.active {
        None => {
            let lines = app
                .system
                .iter()
                .map(|s| Line::from(Span::styled(s.clone(), Style::default().fg(Color::Gray))))
                .collect();
            (" system ".to_string(), lines)
        }
        Some(id) => {
            let Some(p) = app.peers.get(&id) else {
                return (" chat ".to_string(), Vec::new());
            };
            let title = if p.onion.is_empty() {
                format!(" chat · [{id}] (incoming){} ", conn_suffix(p.connected))
            } else {
                format!(
                    " chat · [{id}] {}{} ",
                    crate::app::short_onion(&p.onion),
                    conn_suffix(p.connected)
                )
            };
            let lines = p
                .log
                .iter()
                .map(|c| {
                    let who_style = if c.from_me {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    };
                    Line::from(vec![
                        Span::styled(format!("{}: ", c.who), who_style),
                        Span::raw(c.body.clone()),
                    ])
                })
                .collect();
            (title, lines)
        }
    }
}

fn conn_suffix(connected: bool) -> &'static str {
    if connected {
        ""
    } else {
        " (offline)"
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let widget = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Magenta)),
        Span::raw(app.input.clone()),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" input (Enter=send · /help) "));
    frame.render_widget(widget, area);

    // Place the cursor right after the typed text (account for the "> " prompt + left border).
    let cursor_x = area.x + 1 + 2 + app.input.chars().count() as u16;
    let cursor_y = area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}
