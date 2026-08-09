//! Drawing, and nothing else (design §9.2).
//!
//! No I/O, no state changes, no `await`: this file is a pure function of [`App`],
//! which is what makes a redraw safe to call from anywhere in the loop.
//!
//! The visual vocabulary is the web's (§10.3), deliberately: a status is a symbol
//! **and** a word, never colour alone, and the same three zones sit in the same
//! order so muscle memory carries between the two clients.

use chrono::TimeDelta;
use notify_client_core::types::{Action, State, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use super::compose::ComposeForm;
use super::menu::{Menu, MenuItem};
use super::{ActionItem, App, Pane, completion, sanitise};

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        // Two rows, not one: the hint row alone routinely carries 6-8 items once
        // per-mode bindings are added on top of the always-on ones, more than a
        // single 80-column row holds next to a status message or the `:` line.
        .constraints([Constraint::Min(1), Constraint::Length(2)])
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
    if let Some(menu) = &app.menu {
        self::menu(frame, menu, frame.area());
    }
    if let Some(form) = &app.compose {
        compose(frame, app, form, frame.area());
    }
    // Not while `:` is standing in for a reply's text box (§9.2,
    // `App::reply_for`): free text has no command grammar to suggest against.
    if let Some(command) = &app.command
        && app.reply_for.is_none()
    {
        command_suggestions(frame, command, frame.area());
    }
}

fn channels(frame: &mut Frame, app: &App, area: Rect) {
    // Index 0 is a synthetic "Tous" row, not one of `app.channels` — the one
    // and only way "no filter" is represented (§9.2, `App::applied_channel`),
    // so there is always something to point at even when nothing is filtered.
    let total_unread: u64 = app.unread.values().sum();
    let all_label = if total_unread > 0 {
        format!("Tous ({total_unread})")
    } else {
        "Tous".to_owned()
    };

    let mut items = vec![channel_row(0, app, &all_label)];
    items.extend(app.channels.iter().enumerate().map(|(position, channel)| {
        let index = position + 1;
        // Digits and not a coloured dot: a terminal sidebar has no room for a
        // legend, and "3" says what "•" only hints at (§9.2). Absent from the
        // map means nothing unread, which is why there is no zero branch.
        let label = match app.unread.get(channel.id.as_str()) {
            Some(unread) => format!("{} ({unread})", sanitise(channel.id.as_str())),
            None => sanitise(channel.id.as_str()),
        };
        channel_row(index, app, &label)
    }));

    frame.render_widget(
        List::new(items).block(bordered("Canaux", app.focus == Pane::Channels)),
        area,
    );
}

/// One Channels-pane row. Cursor and applied filter are two independent
/// facts about it — the cursor is today's reversed-style [`selection`], the
/// applied filter a persistent `▸` marker that stays put once the cursor
/// moves on without confirming, so which channel (if any) is active never
/// depends on where the cursor happens to be sitting.
fn channel_row(index: usize, app: &App, label: &str) -> ListItem<'static> {
    let marker = if index == app.applied_channel {
        "▸ "
    } else {
        "  "
    };
    ListItem::new(format!("{marker}{label}")).style(selection(index == app.selected_channel))
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
            // Visible before Détail is even focused: the actions list further
            // down was easy to miss entirely, buried below the body (§9.2).
            if !row.actions.is_empty() {
                spans.push(Span::styled(
                    format!(" · {} actions", row.actions.len()),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans)).style(selection(index == app.selected_row))
        })
        .collect();

    // The filter shown here too, redundantly with the Channels pane's own `▸`
    // marker: a second, always-visible place it is legible without looking at
    // a pane that may not even be focused right now.
    let title = format!("Inbox ({}) · {}", app.inbox.len(), app.filter_label());
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

    let items = app.action_items();
    // Above the fold, unconditionally: the selectable list further down is
    // easy to scroll past without ever noticing it exists (§9.2) — this line
    // is the one place a reader cannot miss that actions are available at all.
    if let Some(summary) = action_summary(&items) {
        lines.push(summary);
    }
    lines.push(Line::from(""));

    if let Some(body) = &row.body {
        // Markdown rendered as plain text: a terminal has no formatter, and §9.2
        // asks for a degraded paragraph rather than a half-parsed one.
        lines.extend(body.lines().map(|line| Line::from(line.to_owned())));
    }

    // Where the first action item's `Line` lands, once the loop below has run
    // — every item from there is exactly one `Line`, in the same order
    // `app.action_items()` lists them, which is what lets the highlighted one
    // be scrolled into view further down.
    let mut action_start = lines.len();
    if !items.is_empty() {
        lines.push(Line::from(""));
        action_start = lines.len();
        for (index, item) in items.iter().enumerate() {
            // Highlighted only once `a` opened the list (`selected_action`
            // `Some`) — a plain, unselected list otherwise, so the buttons are
            // visible before anybody has pressed anything, same as the web
            // client's `ActionBar` (§10.3).
            let highlighted = app.selected_action == Some(index);
            let marker = if highlighted { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{marker}[{}] {}", glyph(item), item.label()),
                selection(highlighted),
            )));
        }
    }

    let scroll = detail_scroll(
        &lines,
        action_start,
        app.selected_action,
        app.detail_scroll,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

/// How far to scroll the Détail pane's body, in wrapped rows.
///
/// Two rules, in order: never scroll past the actual end of a heavily-wrapped
/// body (§9.2 — comparing a *logical* line count against the pane height used
/// to clamp scrolling to near zero long before the real end was visible), and
/// while the action list is open, keep the highlighted item on screen even if
/// that means overriding the pane's own manually-set scroll — `a` opening the
/// list at the top of a long notification, or `j`/`k` walking it past the
/// bottom of the visible window, must not highlight something nobody can see.
///
/// Pure and independently testable — `detail` itself only has to build the
/// `Paragraph` twice (once per probed slice) and pass in the two widths this
/// needs, which is why it takes them rather than an `App`/`Rect`.
fn detail_scroll(
    lines: &[Line<'static>],
    action_start: usize,
    selected_action: Option<usize>,
    requested_scroll: u16,
    inner_width: u16,
    visible_height: u16,
) -> u16 {
    let row_count = |slice: &[Line<'static>]| -> u16 {
        u16::try_from(
            Paragraph::new(slice.to_vec())
                .wrap(Wrap { trim: false })
                .line_count(inner_width),
        )
        .unwrap_or(u16::MAX)
    };

    let total_rows = row_count(lines);
    let max_scroll = total_rows.saturating_sub(visible_height);
    let mut scroll = requested_scroll.min(max_scroll);

    if let Some(selected) = selected_action {
        let item_line = (action_start + selected).min(lines.len());
        let start_row = row_count(&lines[..item_line]);
        let end_row = row_count(&lines[..(item_line + 1).min(lines.len())]);

        if start_row < scroll {
            scroll = start_row;
        } else if end_row > scroll + visible_height {
            scroll = end_row.saturating_sub(visible_height);
        }
        scroll = scroll.min(max_scroll);
    }

    scroll
}

/// Two rows: a hint row that always shows what the current mode's keys do
/// (lazygit-style, §9.2), and below it either the status line or — while the
/// `:` line is open — the command line itself, which is why the split lives
/// here rather than in `draw`: this is the one place that decides which of
/// the two the bottom row is showing.
fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    hint_bar(frame, app, rows[0]);

    if let Some(command) = &app.command {
        command_line(frame, rows[1], command, app.reply_for.is_some());
    } else {
        status_bar(frame, app, rows[1]);
    }
}

fn hint_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mode = if app.holds_stream {
        "flux"
    } else {
        // §9.3bis: another front end holds the stream and feeds the store. Said
        // out loud, because "why is that window live and this one not" is the first
        // question two open windows raise.
        "lecture"
    };

    // Contextual, not a static list (§9.2): `App::footer_hints` mirrors
    // `App::key`'s own priority tiers, so what is shown here is always what
    // pressing that key actually does right now.
    let bindings = app
        .footer_hints()
        .iter()
        .map(|hint| format!("{} {}", hint.key, hint.label))
        .collect::<Vec<_>>()
        .join(" · ");

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {mode} "),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::raw(format!(" {bindings}")),
        ])),
        area,
    );
}

fn status_bar(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Paragraph::new(sanitise(&app.status)), area);
}

fn command_line(frame: &mut Frame, area: Rect, command: &str, replying: bool) {
    // The same line does two different jobs (`App::reply_for`), and looking
    // identical for both would mean typing a reply that reads back as a shell
    // command, or the other way round — a `:` prompt is not what a `reply` action
    // asked for. The active bindings for either mode are the hint row's job now
    // (`App::footer_hints`), not repeated here.
    let glyph = if replying { "↩ " } else { ":" };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(glyph, Style::default().fg(Color::Yellow)),
            Span::raw(command.to_owned()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ])),
        area,
    );
}

fn help(frame: &mut Frame, area: Rect) {
    // Grouped by where each binding applies, the same "several small groups"
    // shape as a lazygit cheat-sheet — and the exact wording `App::footer_hints`
    // uses, so the popup and the live footer can never disagree.
    let text = App::help_sections()
        .into_iter()
        .map(|(section, hints)| {
            let lines = hints
                .iter()
                .map(|hint| format!("  {:<8} {}", hint.key, hint.label))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{section}\n{lines}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    popup(
        frame,
        area,
        "aide",
        &format!("{text}\n\n`:` accepte exactement la grammaire de la CLI (§9)."),
    );
}

/// How many candidates the `:` line's live suggestion box shows at once.
const MAX_SUGGESTIONS: usize = 8;

/// What could come next on the `:` line, shown live as it is typed — the
/// same [`completion::suggestions`] `tab` itself completes from, just made
/// visible without pressing anything (§9.2). Anchored flush above the
/// footer, left-aligned with the `:` prompt below it, so it reads as a
/// dropdown hanging off the line being typed rather than a floating popup.
fn command_suggestions(frame: &mut Frame, command: &str, area: Rect) {
    let candidates = completion::suggestions(command);
    if candidates.is_empty() {
        return;
    }

    let shown = candidates.len().min(MAX_SUGGESTIONS);
    let width = area.width.saturating_sub(4).clamp(20, 40);
    let height = u16::try_from(shown).unwrap_or(u16::MAX).saturating_add(2);
    let region = Rect {
        x: area.x,
        // Two rows of footer below this: the hint bar and the `:` line itself.
        y: area.height.saturating_sub(2 + height),
        width,
        height,
    };

    let items: Vec<ListItem> = candidates
        .iter()
        .take(shown)
        .map(|candidate| ListItem::new(candidate.clone()))
        .collect();
    let title = if candidates.len() > shown {
        format!("Suggestions ({}+)", candidates.len())
    } else {
        "Suggestions".to_owned()
    };

    frame.render_widget(ratatui::widgets::Clear, region);
    frame.render_widget(List::new(items).block(bordered(&title, false)), region);
}

/// The guided channel/dnd/device menu (§9.2) — a discoverability layer over
/// commands nobody stumbles onto by trying keys, drawn with the same
/// `bordered`/`ListItem`/`selection` vocabulary as the Channels pane.
fn menu(frame: &mut Frame, menu: &Menu, area: Rect) {
    let width = area.width.saturating_sub(20).clamp(24, 60);
    let height = u16::try_from(menu.items().len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height.saturating_sub(4));
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let items: Vec<ListItem> = menu
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let glyph = if matches!(item, MenuItem::Group { .. }) {
                "▸"
            } else {
                " "
            };
            ListItem::new(format!("{glyph} {}", item.label()))
                .style(selection(index == menu.cursor()))
        })
        .collect();

    frame.render_widget(ratatui::widgets::Clear, region);
    frame.render_widget(List::new(items).block(bordered("Menu", true)), region);
}

/// The guided compose/publish form (§9.2) — one row per field, cyclable ones
/// shown as `‹ value ›` so `h`/`l` is self-evident without reading the footer,
/// the focused text field carrying a cursor block while it is being edited.
fn compose(frame: &mut Frame, app: &App, form: &ComposeForm, area: Rect) {
    let rows = form.rows(&app.channels);
    let width = area.width.saturating_sub(10).clamp(40, 76);
    let height = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height.saturating_sub(4));
    let region = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let focused = index == form.focused();
            let line = if row.is_submit {
                Line::from(Span::styled(
                    format!("  [ {} ]", row.label),
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            } else {
                let value = if row.cyclable {
                    format!("‹ {} ›", row.value)
                } else if focused && form.is_editing() {
                    format!("{}█", row.value)
                } else {
                    row.value.clone()
                };
                Line::from(format!("  {:<10} {value}", row.label))
            };
            ListItem::new(line).style(selection(focused))
        })
        .collect();

    frame.render_widget(ratatui::widgets::Clear, region);
    frame.render_widget(
        List::new(items).block(bordered("Publier une notification", true)),
        region,
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

/// A one-line, always-visible summary of what a notification's actions are,
/// shown above the body rather than only in the scrollable, `a`-triggered
/// list further down (§9.2 — actions were easy to miss entirely before this).
/// `None` for a notification with none, same "closed and empty read the same
/// way" rule `selected_action: Option<usize>` already follows.
fn action_summary(items: &[ActionItem]) -> Option<Line<'static>> {
    if items.is_empty() {
        return None;
    }

    let summary = items
        .iter()
        .map(|item| format!("{} {}", glyph(item), item.label()))
        .collect::<Vec<_>>()
        .join("   ");

    Some(Line::from(format!("Actions : {summary}")))
}

/// A glyph per action kind, so the list reads at a glance without the label
/// alone having to carry it (§10.3, the same "symbol and word" rule as status).
const fn glyph(item: &ActionItem) -> &'static str {
    match item {
        ActionItem::Whole(Action::View { .. }) => "↗",
        ActionItem::Whole(Action::Ack { .. }) => "✓",
        ActionItem::Whole(Action::Reply { .. }) => "…",
        ActionItem::Whole(Action::Choice { .. }) => "▾",
        ActionItem::Whole(Action::Silence { .. }) => "⏾",
        ActionItem::ChoiceOption { .. } => "·",
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
    #![allow(clippy::expect_used, reason = "a failing assertion is a failing test")]

    use notify_client_core::types::ActionKey;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::app::Row;
    use super::*;

    #[test]
    fn detail_scroll_uses_wrapped_rows_not_a_logical_line_count_for_its_bound() {
        // The bug this pins: comparing a *logical* line count against the
        // pane height clamped scrolling to (near) zero long before the real
        // end of a heavily-wrapped body was ever visible.
        let lines: Vec<Line<'static>> = vec![Line::from("mot ".repeat(300))];

        let scroll = detail_scroll(&lines, lines.len(), None, u16::MAX, 18, 8);

        assert!(
            scroll > 1,
            "one long logical line must still allow scrolling well past what \
             its logical line count (1) would have allowed"
        );
    }

    #[test]
    fn detail_scroll_scrolls_down_to_reveal_a_later_selected_action() {
        let mut lines: Vec<Line<'static>> =
            (0..20).map(|i| Line::from(format!("filler {i}"))).collect();
        let action_start = lines.len();
        lines.push(Line::from("[ ] first"));
        lines.push(Line::from("[ ] second"));

        // Manually scrolled to the very top; opening the list highlights the
        // second action (row 21 of 22), well below an 8-row viewport.
        let scroll = detail_scroll(&lines, action_start, Some(1), 0, 40, 8);

        assert_eq!(
            scroll, 14,
            "the second action must become the last visible row of the viewport"
        );
    }

    #[test]
    fn detail_scroll_scrolls_back_up_to_reveal_an_earlier_selected_action() {
        // Five one-row actions and a 3-row viewport: scrolled to the very
        // bottom shows only the last three (2, 3, 4) — action 0 is above it.
        let lines: Vec<Line<'static>> = (0..5)
            .map(|i| Line::from(format!("[ ] action {i}")))
            .collect();

        let scroll = detail_scroll(&lines, 0, Some(0), 2, 40, 3);

        assert_eq!(
            scroll, 0,
            "selecting the first action must scroll back up to reveal it, \
             even though the pane was already scrolled toward the end"
        );
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn typing_a_partial_command_shows_matching_candidates() {
        let area = Rect::new(0, 0, 40, 12);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("a test backend");

        terminal
            .draw(|frame| command_suggestions(frame, "pub", area))
            .expect("must draw without panicking");

        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("publish"),
            "typing `pub` must surface `publish` as a candidate, live, without tab"
        );
    }

    #[test]
    fn a_line_with_no_matching_command_shows_no_suggestion_box() {
        let area = Rect::new(0, 0, 40, 12);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("a test backend");

        terminal
            .draw(|frame| command_suggestions(frame, "zzz", area))
            .expect("must draw without panicking");

        let rendered = rendered_text(&terminal);
        assert!(
            !rendered.contains("Suggestions"),
            "no candidates means nothing to show, not an empty box"
        );
    }

    #[test]
    fn the_suggestion_box_narrows_as_a_subcommand_is_typed() {
        // Contextual, same as `tab` itself: past `channel `, the candidates
        // are its own subcommands, not the root command list.
        let area = Rect::new(0, 0, 40, 12);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("a test backend");

        terminal
            .draw(|frame| command_suggestions(frame, "channel ", area))
            .expect("must draw without panicking");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("create"));
        assert!(
            !rendered.contains("publish"),
            "a root command is not a channel subcommand"
        );
    }

    #[test]
    fn a_heavily_wrapped_body_can_still_be_scrolled_to_its_end() {
        // The bug this pins: the old bound compared the *logical* line count
        // against the pane height, not the *wrapped* row count. A body written
        // as a handful of long lines wraps into many more on-screen rows than
        // that at a narrow pane width, so `detail_scroll.min(max_scroll)`
        // clamped to (near) zero and scrolling down did nothing, well before
        // the actual end of the body was ever visible.
        // One logical line — `body.lines()` sees exactly one — that wraps
        // into far more than the ~8 rows an 8-tall pane shows at once. The
        // marker sits at the very end, past what the *logical* line count
        // (title + meta + revision + id + blank + this one line = 6) would
        // ever have let the old bound scroll to.
        let long_body = format!("{}FIN_MARQUEUR", "mot ".repeat(300));
        let mut app = App::new("default".to_owned(), true);
        app.focus = Pane::Detail;
        app.inbox = vec![Row {
            id: "ntf_1".to_owned(),
            channel: "ci-infra".to_owned(),
            title: "etcd degraded".to_owned(),
            body: Some(long_body),
            seq: 1,
            last_seq: 1,
            state: State::Open,
            status: Status::Failure,
            late_by: None,
            muted_by_dnd: false,
            actions: Vec::new(),
        }];
        // Ask for as much scroll as there is; a correct bound clamps this
        // down to exactly "the end", not to whatever the logical count allows.
        app.detail_scroll = u16::MAX;

        let area = Rect::new(0, 0, 20, 10);
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("a test backend");
        terminal
            .draw(|frame| detail(frame, &app, area))
            .expect("detail must draw without panicking");

        let rendered = rendered_text(&terminal);

        assert!(
            rendered.contains("FIN_MARQUEUR"),
            "scrolling to the end of a long wrapped body must reveal its last word, \
             not stay clamped near the top by an under-counted scroll bound"
        );
    }

    #[test]
    fn a_delay_is_rendered_exactly_the_way_7_2_writes_it() {
        assert_eq!(elapsed(TimeDelta::minutes(133)), "2h13");
        assert_eq!(elapsed(TimeDelta::minutes(7)), "7min");
        assert_eq!(elapsed(TimeDelta::minutes(61)), "1h01");
        assert_eq!(elapsed(TimeDelta::days(3)), "3j");
    }

    #[test]
    fn action_summary_is_none_when_there_are_no_actions() {
        assert!(action_summary(&[]).is_none());
    }

    #[test]
    fn action_summary_lists_every_action_with_its_label() {
        // The bug this fixes: actions rendered only below the body, in a
        // scrollable pane, were easy to miss entirely (§9.2). This line has to
        // be visible above the fold and to name every action, not just count them.
        let items = vec![
            ActionItem::Whole(Action::Ack {
                key: ActionKey::from("ack".to_owned()),
                label: "Je m'en occupe".to_owned(),
            }),
            ActionItem::Whole(Action::View {
                label: "Voir le run".to_owned(),
                url: "https://example.test".to_owned(),
            }),
        ];

        let text = action_summary(&items)
            .expect("a summary once there are actions")
            .to_string();

        assert!(text.starts_with("Actions :"));
        assert!(text.contains("Je m'en occupe"));
        assert!(text.contains("Voir le run"));
    }
}
