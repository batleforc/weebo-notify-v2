//! Drawing, and nothing else (design §9.2).
//!
//! No I/O, no state changes, no `await`: this file is a pure function of [`App`],
//! which is what makes a redraw safe to call from anywhere in the loop.
//!
//! The visual vocabulary is the web's (§10.3), deliberately: a status is a symbol
//! **and** a word, never colour alone, and the same three zones sit in the same
//! order so muscle memory carries between the two clients.

use chrono::TimeDelta;
use notify_client_core::types::{State, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use super::{App, BINDINGS, Pane, sanitise};

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Min(30),
            Constraint::Length(40),
        ])
        .split(rows[0]);

    channels(frame, app, columns[0]);
    inbox(frame, app, columns[1]);
    detail(frame, app, columns[2]);
    footer(frame, app, rows[1]);

    if app.show_help {
        help(frame, frame.area());
    }
    if let Some(output) = &app.output {
        popup(frame, frame.area(), "sortie", output);
    }
    if let Some(command) = &app.command {
        command_line(frame, rows[1], command);
    }
}

fn channels(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .channels
        .iter()
        .enumerate()
        .map(|(index, channel)| {
            let label = sanitise(channel.id.as_str());
            ListItem::new(label).style(selection(index == app.selected_channel))
        })
        .collect();

    frame.render_widget(
        List::new(items).block(bordered("Canaux", app.focus == Pane::Channels)),
        area,
    );
}

fn inbox(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .inbox
        .iter()
        .enumerate()
        .map(|(index, row)| {
            // Symbol *and* word: colour alone excludes a portion of the people
            // reading an incident feed (§10.3).
            let mark = Span::styled(
                format!("{} {:<8}", symbol(row.status), word(row.status)),
                Style::default().fg(colour(row.status)),
            );
            let mut spans = vec![
                mark,
                Span::raw(format!("{} ", live(row.state))),
                Span::raw(row.title.clone()),
            ];
            // §7.2: a notification that reached this machine long after it was
            // published is not the same as a fresh one, and saying so is the
            // difference between "nothing happened" and "you were offline".
            if let Some(late) = row.late_by.filter(|late| *late >= LATE) {
                spans.push(Span::styled(
                    format!(" reçue en retard (+{})", elapsed(late)),
                    Style::default().fg(Color::Yellow),
                ));
            }
            ListItem::new(Line::from(spans)).style(selection(index == app.selected_row))
        })
        .collect();

    let title = format!("Inbox ({})", app.inbox.len());
    frame.render_widget(
        List::new(items).block(bordered(&title, app.focus == Pane::Inbox)),
        area,
    );
}

fn detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = bordered("Détail", app.focus == Pane::Detail);

    let Some(row) = app.current() else {
        frame.render_widget(
            Paragraph::new("Sélectionne une notification.").block(block),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(Span::styled(
            row.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{} · {} · {}",
            word(row.status),
            state_word(row.state),
            row.channel
        )),
        Line::from(format!("révision {} sur {}", row.seq, row.last_seq)),
        // The id, because it is what every other command takes: `notify update -i`,
        // a `/n/<id>` permalink in the web client (§10.4), an interaction call.
        Line::from(Span::styled(
            row.id.clone(),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    if let Some(late) = row.late_by.filter(|late| *late >= LATE) {
        lines.push(Line::from(Span::styled(
            format!("reçue en retard (+{})", elapsed(late)),
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));

    if let Some(body) = &row.body {
        // Markdown rendered as plain text: a terminal has no formatter, and §9.2
        // asks for a degraded paragraph rather than a half-parsed one.
        lines.extend(body.lines().map(|line| Line::from(line.to_owned())));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let mode = if app.holds_stream {
        "flux"
    } else {
        // §9.3bis: another front end holds the stream and feeds the store. Said
        // out loud, because "why is that window live and this one not" is the first
        // question two open windows raise.
        "lecture"
    };

    let bindings = BINDINGS
        .iter()
        .map(|(key, what)| format!("{key} {what}"))
        .collect::<Vec<_>>()
        .join(" · ");

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {mode} "),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::raw(format!(" {bindings}  {}", sanitise(&app.status))),
        ])),
        area,
    );
}

fn command_line(frame: &mut Frame, area: Rect, command: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(":", Style::default().fg(Color::Yellow)),
            Span::raw(command.to_owned()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            // The bindings this line has and the rest of the interface does not.
            // §9.2 wants the active bindings visible at all times, and completion
            // nobody knows about is completion nobody uses.
            Span::styled(
                "   tab compléter · ↑↓ historique · esc annuler",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        area,
    );
}

fn help(frame: &mut Frame, area: Rect) {
    let text = BINDINGS
        .iter()
        .map(|(key, what)| format!("  {key:<8} {what}"))
        .collect::<Vec<_>>()
        .join("\n");

    popup(
        frame,
        area,
        "aide",
        &format!("{text}\n\n  `:` accepte exactement la grammaire de la CLI (§9)."),
    );
}

fn popup(frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let width = area.width.saturating_sub(10).clamp(20, 80);
    let height = area.height.saturating_sub(6).clamp(6, 20);
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(ratatui::widgets::Clear, region);
    frame.render_widget(
        Paragraph::new(sanitise(body))
            .block(bordered(title, true))
            .wrap(Wrap { trim: false }),
        region,
    );
}

fn bordered(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        })
}

const fn selection(selected: bool) -> Style {
    if selected {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    }
}

/// A glyph per status. Paired with a word everywhere it is used (§10.3).
const fn symbol(status: Status) -> &'static str {
    match status {
        Status::Success => "✓",
        Status::Failure => "✗",
        Status::Warning => "!",
        Status::Info => "•",
        Status::Cancelled => "-",
    }
}

const fn word(status: Status) -> &'static str {
    match status {
        Status::Success => "succès",
        Status::Failure => "échec",
        Status::Warning => "alerte",
        Status::Info => "info",
        Status::Cancelled => "annulé",
    }
}

const fn colour(status: Status) -> Color {
    match status {
        Status::Success => Color::Green,
        Status::Failure => Color::Red,
        Status::Warning => Color::Yellow,
        Status::Info => Color::Blue,
        Status::Cancelled => Color::DarkGray,
    }
}

/// A live notification gets a mark; a closed one is still (§10.3).
const fn live(state: State) -> &'static str {
    match state {
        State::Open => "○",
        State::Working => "◐",
        State::Closed => " ",
    }
}

const fn state_word(state: State) -> &'static str {
    match state {
        State::Open => "ouverte",
        State::Working => "en cours",
        State::Closed => "fermée",
    }
}

/// Below this, "late" is clock skew and network latency rather than a wait worth
/// a badge (design §7.2 — the example there is `+2h13`).
const LATE: TimeDelta = TimeDelta::minutes(5);

/// A delay, in the shortest form that is still exact enough to act on: `2h13`,
/// `3j`, `7min`. Never "a while ago" — the number is the point.
fn elapsed(late: TimeDelta) -> String {
    let minutes = late.num_minutes();

    match minutes {
        ..60 => format!("{minutes}min"),
        // Hours *and* minutes, because §7.2's own example is `+2h13`: the minutes
        // are what make it read as a measurement rather than a rounding.
        60..1_440 => format!("{}h{:02}", minutes / 60, minutes % 60),
        _ => format!("{}j", minutes / 1_440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delay_is_rendered_exactly_the_way_7_2_writes_it() {
        assert_eq!(elapsed(TimeDelta::minutes(133)), "2h13");
        assert_eq!(elapsed(TimeDelta::minutes(7)), "7min");
        assert_eq!(elapsed(TimeDelta::minutes(61)), "1h01");
        assert_eq!(elapsed(TimeDelta::days(3)), "3j");
    }
}
