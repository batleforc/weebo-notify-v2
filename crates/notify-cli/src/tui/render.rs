//! Drawing only (§9.2): a pure function of [`App`]. Statuses are a symbol and a word, never
//! colour alone, in the web's three-zone layout (§10.3).

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
    let [main, bottom] = Layout::default()
        .direction(Direction::Vertical)
        // Two rows: the hint row alone often outgrows 80 columns.
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .areas(frame.area());

    let [left, middle, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Min(30),
            Constraint::Length(40),
        ])
        .areas(main);

    channels(frame, app, left);
    inbox(frame, app, middle);
    detail(frame, app, right);
    footer(frame, app, bottom);

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
    // Not while `:` stands in for a reply's text box: free text has nothing to suggest.
    if let Some(command) = &app.command
        && app.reply_for.is_none()
    {
        command_suggestions(frame, command, frame.area());
    }
}

fn channels(frame: &mut Frame, app: &App, area: Rect) {
    // Index 0 is the synthetic "Tous" row: "no filter" (`App::applied_channel`).
    let total_unread: u64 = app.unread.values().sum();
    let all_label = if total_unread > 0 {
        format!("Tous ({total_unread})")
    } else {
        "Tous".to_owned()
    };

    let mut items = vec![channel_row(0, app, &all_label)];
    items.extend(app.channels.iter().enumerate().map(|(position, channel)| {
        let index = position + 1;
        // Digits, not a dot (§9.2); absent from the map means nothing unread.
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

/// One Channels-pane row: the cursor is the reversed [`selection`] style, the applied filter
/// a persistent `▸` marker, independent of each other.
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
            // Symbol and word: colour alone excludes some readers (§10.3).
            let mark = Span::styled(
                format!("{} {:<8}", symbol(row.status), word(row.status)),
                Style::default().fg(colour(row.status)),
            );
            let mut spans = vec![
                mark,
                Span::raw(format!("{} ", live(row.state))),
                Span::raw(row.title.clone()),
            ];
            // §7.2: badge a notification that arrived long after it was published.
            if let Some(late) = row.late_by.filter(|late| *late >= LATE) {
                spans.push(Span::styled(
                    format!(" reçue en retard (+{})", elapsed(late)),
                    Style::default().fg(Color::Yellow),
                ));
            }
            // Visible before Détail is focused (§9.2).
            if !row.actions.is_empty() {
                spans.push(Span::styled(
                    format!(" · {} actions", row.actions.len()),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans)).style(selection(index == app.selected_row))
        })
        .collect();

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
        // The id every other command takes (`notify update -i`, `/n/<id>`, interactions).
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
    // Above the fold, so nobody misses that actions exist (§9.2).
    if let Some(summary) = action_summary(&items) {
        lines.push(summary);
    }
    lines.push(Line::from(""));

    if let Some(body) = &row.body {
        // Markdown as plain text: a terminal has no formatter (§9.2).
        lines.extend(body.lines().map(|line| Line::from(line.to_owned())));
    }

    // Where the first action `Line` lands; one `Line` per item, in `app.action_items()` order.
    let mut action_start = lines.len();
    if !items.is_empty() {
        lines.push(Line::from(""));
        action_start = lines.len();
        for (index, item) in items.iter().enumerate() {
            // Highlighted only once `a` opened the list; visible regardless, like the web's `ActionBar`.
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

/// How far to scroll the Détail pane, in wrapped rows: never past the real (wrapped) end,
/// and while the action list is open, keep the highlighted item on screen.
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
        // Both bounds are clamped to `lines.len()` above, so `get` always answers.
        let start_row = lines.get(..item_line).map_or(0, row_count);
        let end_row = lines
            .get(..(item_line + 1).min(lines.len()))
            .map_or(0, row_count);

        if start_row < scroll {
            scroll = start_row;
        } else if end_row > scroll + visible_height {
            scroll = end_row.saturating_sub(visible_height);
        }
        scroll = scroll.min(max_scroll);
    }

    scroll
}

/// Two rows: a contextual key-hint row (§9.2), and below it the status line or the `:` line.
fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let [hints, line] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .areas(area);

    hint_bar(frame, app, hints);

    if let Some(command) = &app.command {
        command_line(frame, line, command, app.reply_for.is_some());
    } else {
        status_bar(frame, app, line);
    }
}

fn hint_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mode = if app.holds_stream {
        "flux"
    } else {
        // §9.3bis: another front end holds the stream; say so.
        "lecture"
    };

    // `App::footer_hints` mirrors `App::key`'s priority tiers, so hints match what keys do now.
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
    // Different glyph for a reply vs. a command, since the same line does both.
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
    // Same wording as `App::footer_hints`, so the popup and footer agree.
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

/// Live `:` suggestions from [`completion::suggestions`] (§9.2), anchored above the footer
/// under the prompt like a dropdown.
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
        // Two footer rows below: the hint bar and the `:` line.
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

/// The guided channel/dnd/device menu (§9.2).
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

/// The guided compose form (§9.2): cyclable fields shown as `‹ value ›`, the edited field with a cursor.
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

/// A one-line summary of a notification's actions, shown above the body (§9.2); `None` without any.
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

/// A glyph per action kind, paired with the label (§10.3).
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

/// Below this, "late" is clock skew rather than a wait worth a badge (§7.2).
const LATE: TimeDelta = TimeDelta::minutes(5);

/// A delay in the shortest exact form: `2h13`, `3j`, `7min`.
fn elapsed(late: TimeDelta) -> String {
    let minutes = late.num_minutes();

    match minutes {
        ..60 => format!("{minutes}min"),
        // Hours and minutes, as §7.2's `+2h13`.
        60..1_440 => format!("{}h{:02}", minutes / 60, minutes % 60),
        _ => format!("{}j", minutes / 1_440),
    }
}

#[cfg(test)]
mod tests {
    use notify_client_core::types::ActionKey;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::app::Row;
    use super::*;

    #[test]
    fn detail_scroll_uses_wrapped_rows_not_a_logical_line_count_for_its_bound() {
        // Must scroll to the wrapped end, not stop at the logical line count.
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

        // Scrolled to the top; the second action (row 21 of 22) is below an 8-row viewport.
        let scroll = detail_scroll(&lines, action_start, Some(1), 0, 40, 8);

        assert_eq!(
            scroll, 14,
            "the second action must become the last visible row of the viewport"
        );
    }

    #[test]
    fn detail_scroll_scrolls_back_up_to_reveal_an_earlier_selected_action() {
        // Five one-row actions, 3-row viewport, scrolled to the bottom: action 0 is above it.
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
        // Past `channel `, the candidates are its own subcommands.
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
        // One logical line wrapping far past an 8-tall pane, with a marker at the very end that a
        // logical-line-count bound would never scroll to.
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
        // Ask for maximal scroll; a correct bound clamps to exactly the end.
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
        // Must be above the fold and name every action, not just count them.
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
