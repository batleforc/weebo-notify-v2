//! The state, the messages, and the only function allowed to change one into the
//! other (design §9.2).

use std::collections::BTreeMap;

use chrono::TimeDelta;
use crossterm::event::{KeyCode, KeyModifiers};
use notify_client_core::types::{Channel, Notification, State, Status};
use notify_client_core::{CachedNotification, Session, Store};

use super::history;
use super::sanitise;

/// One line of the inbox, whichever side it came from.
///
/// The two sources are not interchangeable and that is the point of this type: a
/// front end holding the stream renders the *server's* answer, one that lost the
/// election renders the **local store** (§9.3bis) — and the drawing code must not
/// have to know which, or it would grow two of everything.
///
/// Text is sanitised here rather than at each `draw` (§2.3): a title reaches this
/// struct once and is drawn on every frame.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: String,
    pub channel: String,
    pub title: String,
    pub body: Option<String>,
    pub seq: i32,
    /// The newest revision the source knows about. Equal to `seq` when the source
    /// is the cache, which only ever holds the latest one.
    pub last_seq: i32,
    pub state: State,
    pub status: Status,
    /// How long after its creation this profile first saw it (§7.2). `None` when
    /// the local store never recorded an arrival — which is not the same as zero.
    pub late_by: Option<TimeDelta>,
    /// Whether the **server** said this one landed inside a quiet period (§7.4).
    ///
    /// Not re-derived here, and that is the point of D22: the rule lives where the
    /// preference does, the server applies it per reader, and every client that
    /// answered the question locally was another copy to keep in step.
    pub muted_by_dnd: bool,
}

impl Row {
    /// From the server's answer, with the arrival time the local store remembers.
    ///
    /// The badge needs both halves and neither side has both: the server knows when
    /// it was created, only this machine knows when it got here.
    fn from_inbox(entry: &Notification, late_by: Option<TimeDelta>) -> Self {
        let head = &entry.head;
        Self {
            id: entry.id.to_string(),
            channel: sanitise(entry.channel.as_str()),
            title: sanitise(&head.content.title),
            body: head.content.body.as_deref().map(sanitise),
            seq: head.seq,
            last_seq: entry.last_seq,
            state: head.state,
            status: head.status,
            late_by,
            // `GET /inbox` is a list of notifications, not of deliveries: it has no
            // opinion about a quiet period, and the store does (§7.4). A row read
            // from the server is therefore never *known* to be muted — which is the
            // safe direction, and the reason a popup decision reads the cache path.
            muted_by_dnd: false,
        }
    }

    /// From the local cache, which is what a reader draws.
    fn from_cache(cached: CachedNotification) -> Self {
        let late_by = cached.late_by();
        // The generated client renders `seq` as `i32` and the cache holds the `u32`
        // it really is; a sequence that overflowed would be a server bug long before
        // it were a display one, so it saturates rather than wrapping to a negative.
        let seq = i32::try_from(cached.seq).unwrap_or(i32::MAX);
        Self {
            id: cached.id,
            channel: sanitise(&cached.channel),
            title: sanitise(&cached.title),
            body: cached.body.as_deref().map(sanitise),
            seq,
            last_seq: seq,
            state: cached.state,
            status: cached.status,
            late_by,
            muted_by_dnd: cached.deferred_by_dnd,
        }
    }
}

/// Which of the three zones has focus (§9.2, §10.4 — the same layout as the web).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Channels,
    Inbox,
    Detail,
}

impl Pane {
    const fn next(self) -> Self {
        match self {
            Self::Channels => Self::Inbox,
            Self::Inbox => Self::Detail,
            Self::Detail => Self::Channels,
        }
    }
}

/// Everything that can change the state.
///
/// An enum rather than direct calls, because it is what makes the state machine
/// testable: a key, a tick and an API answer all arrive the same way, and `update`
/// is the only place that reacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    /// Nothing happened; advance the clock and poll the store.
    Tick,
    /// The front end holding the stream wrote to the store (§9.3bis). Reload from
    /// it — without asking the server, which is what the shared store is for.
    StoreChanged,
    /// Move the selection in the focused pane.
    Move(i32),
    /// Move focus.
    NextPane,
    /// Ask the server again.
    Refresh,
    /// Open the command line.
    OpenCommand,
    /// A character typed into the command line.
    CommandChar(char),
    /// Backspace in the command line.
    CommandBackspace,
    /// Run what is in the command line.
    RunCommand,
    /// Complete what is typed from the clap tree (§9.2).
    CompleteCommand,
    /// Walk the persisted history: -1 is older, +1 is newer (§9.2).
    RecallCommand(i32),
    /// Leave the command line, or close the popup.
    Cancel,
    /// Toggle the help popup.
    ToggleHelp,
    /// Leave.
    Quit,
    /// A key with nothing bound to it.
    Ignored,
}

/// What the interface is showing.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent facts about one screen — which role this window has, \
              whether a popup is open, whether it is closing, whether it owes the \
              readers a poke. Grouping them into a state enum would make the \
              impossible combinations unrepresentable and the possible ones unreadable"
)]
pub(crate) struct App {
    pub profile: String,
    /// Whether this front end holds the stream, or reads what another one writes
    /// (§9.3bis). Shown, because "why is this one live and that one not" is the
    /// first question somebody asks with two windows open.
    pub holds_stream: bool,
    pub channels: Vec<Channel>,
    /// Unread per channel id (§7.3, §9.2) — the sidebar badge.
    ///
    /// A map and not a field on [`Row`]: the count is a property of the channel,
    /// and the server omits the channels with nothing unread, so an absent key and
    /// a zero mean the same thing here by construction.
    pub unread: BTreeMap<String, u64>,
    pub inbox: Vec<Row>,
    pub focus: Pane,
    pub selected_channel: usize,
    pub selected_row: usize,
    /// The `:` line, when it is open.
    pub command: Option<String>,
    /// What the `:` line remembers between runs (§9.2). Loaded from the profile
    /// directory, so a TUI closed and reopened keeps yesterday's lines.
    pub history: history::History,
    /// What the last command printed, shown in a popup.
    pub output: Option<String>,
    pub show_help: bool,
    pub status: String,
    pub should_quit: bool,
    /// Whether the inbox has ever been filled, arrival or no arrival.
    ///
    /// The first load is not a pile of arrivals (a TUI that opens by popping the
    /// whole inbox is one nobody opens twice), and "the inbox is empty" is not the
    /// same question — see [`Self::note_arrivals`].
    pub loaded: bool,
    /// Rows that appeared since the last time the loop looked, oldest first.
    ///
    /// Filled by whatever reloads the inbox and drained by the loop, for the same
    /// reason `wrote` is: `update` stays free of I/O, and popping a desktop
    /// notification is I/O (§9.2).
    pub arrivals: Vec<Row>,
    /// Set when this front end just wrote to the shared store, and cleared by the
    /// loop once it has poked the readers (§9.3bis). A flag rather than a call from
    /// `update`, because `update` is the one function that must stay free of I/O.
    pub wrote: bool,
}

impl App {
    pub(crate) fn new(profile: String, holds_stream: bool) -> Self {
        let history = history::History::load(Some(history::history_path(&profile)));
        Self {
            profile,
            holds_stream,
            channels: Vec::new(),
            unread: BTreeMap::new(),
            inbox: Vec::new(),
            focus: Pane::Inbox,
            selected_channel: 0,
            selected_row: 0,
            command: None,
            history,
            output: None,
            show_help: false,
            status: String::new(),
            loaded: false,
            arrivals: Vec::new(),
            should_quit: false,
            wrote: false,
        }
    }

    /// What arrived since the last call, clearing it as it answers.
    pub(crate) fn take_arrivals(&mut self) -> Vec<Row> {
        std::mem::take(&mut self.arrivals)
    }

    /// Whether readers should be poked, clearing the flag as it answers.
    pub(crate) fn take_wrote(&mut self) -> bool {
        std::mem::take(&mut self.wrote)
    }

    /// Turn a key into a message.
    ///
    /// Separate from [`Self::update`] so the bindings can be tested without a
    /// terminal, and so the command line can swallow keys without every binding
    /// having to check whether it is open.
    pub(crate) fn key(&self, code: KeyCode, modifiers: KeyModifiers) -> Message {
        if self.command.is_some() {
            return match code {
                KeyCode::Esc => Message::Cancel,
                KeyCode::Enter => Message::RunCommand,
                KeyCode::Backspace => Message::CommandBackspace,
                KeyCode::Tab => Message::CompleteCommand,
                KeyCode::Up => Message::RecallCommand(-1),
                KeyCode::Down => Message::RecallCommand(1),
                KeyCode::Char(character) => Message::CommandChar(character),
                _ => Message::Ignored,
            };
        }

        if self.output.is_some() || self.show_help {
            return match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        match code {
            KeyCode::Char('q') => Message::Quit,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Message::Quit,
            KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
            KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
            // Enter and Tab both move focus: Enter reads as "open this" on a row,
            // and the row is already selected, so opening it *is* focusing the
            // detail pane.
            KeyCode::Tab | KeyCode::Enter => Message::NextPane,
            KeyCode::Char('r') => Message::Refresh,
            KeyCode::Char(':') => Message::OpenCommand,
            KeyCode::Char('?') => Message::ToggleHelp,
            _ => Message::Ignored,
        }
    }

    /// The only place state changes.
    pub(crate) async fn update(&mut self, message: Message, session: &Session, store: &Store) {
        match message {
            Message::Quit => self.should_quit = true,
            Message::NextPane => self.focus = self.focus.next(),
            Message::Move(delta) => self.move_selection(delta),
            Message::ToggleHelp => self.show_help = !self.show_help,
            Message::Cancel => {
                self.command = None;
                self.output = None;
                self.show_help = false;
            }
            Message::OpenCommand => self.command = Some(String::new()),
            Message::CommandChar(character) => {
                if let Some(line) = &mut self.command {
                    line.push(character);
                }
            }
            Message::CommandBackspace => {
                if let Some(line) = &mut self.command {
                    line.pop();
                }
            }
            Message::CompleteCommand => {
                if let Some(line) = &mut self.command
                    && let Some(completed) = super::completion::complete(line)
                {
                    *line = completed;
                }
            }
            Message::RecallCommand(direction) => {
                if self.command.is_some() {
                    let recalled = if direction < 0 {
                        self.history.previous().map(ToOwned::to_owned)
                    } else {
                        self.history.next().map(ToOwned::to_owned)
                    };
                    // Nothing newer than the newest is the line the caller was
                    // typing, and an empty line is the honest rendering of it.
                    self.command = Some(recalled.unwrap_or_default());
                }
            }
            Message::RunCommand => {
                let line = self.command.take().unwrap_or_default();
                self.history.push(&line);
                self.output = Some(run_command(&line, &self.profile, session).await);
                // Whatever it did, the server may now disagree with the screen.
                self.refresh(session, store).await;
            }
            Message::Refresh => self.refresh(session, store).await,
            // The holder wrote: reload from the store and *not* from the server. A
            // reader that answered a poke with a request would spend the connection
            // budget the election exists to protect (§8.2, §9.3bis).
            Message::StoreChanged => self.reload_from_store(store),
            // Nothing to do for either: a tick exists to let the loop repaint, and
            // an unbound key is deliberately silent rather than beeping.
            Message::Tick | Message::Ignored => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let (index, len) = match self.focus {
            Pane::Channels => (&mut self.selected_channel, self.channels.len()),
            Pane::Inbox | Pane::Detail => (&mut self.selected_row, self.inbox.len()),
        };

        if len == 0 {
            return;
        }

        // Saturating on both ends rather than casting through a signed type: a
        // selection that wrapped to `usize::MAX` would index nothing and paint an
        // empty detail pane with no explanation.
        *index = if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (*index + delta.unsigned_abs() as usize).min(len - 1)
        };
    }

    /// Ask the server for both lists, and feed the store if this front end holds it.
    pub(crate) async fn refresh(&mut self, session: &Session, store: &Store) {
        match session.channels().await {
            Ok(channels) => self.channels = channels,
            Err(error) => self.status = format!("canaux: {error}"),
        }

        // A badge that fails to load leaves the sidebar without one rather than
        // taking the frame down with it: the channel list and the inbox are what
        // this screen is for, and a stale count is worse than no count.
        match session.unread().await {
            Ok(counts) => {
                // `u64::try_from` and not a cast: the generated client widens the
                // schema's unsigned count to `i64` (it is a boundary, not an
                // interface — see `notify_client_core`), and an entry that failed
                // to be a positive count is dropped rather than rendered as `(0)`.
                // That keeps this map's invariant — every key is a badge worth
                // drawing — which is the same one the port states server-side.
                self.unread = counts
                    .into_iter()
                    .filter_map(|count| {
                        u64::try_from(count.unread)
                            .ok()
                            .filter(|unread| *unread > 0)
                            .map(|unread| (count.channel.to_string(), unread))
                    })
                    .collect();
            }
            Err(error) => self.status = format!("non-lus: {error}"),
        }

        // Only the holder writes: two front ends syncing the same cursor would
        // fight over it, and the loser would rewind the winner (§9.3bis). Done
        // *before* reading the inbox so a catch-up is visible in this same frame.
        if self.holds_stream {
            match store.device() {
                Ok(device) => {
                    match session
                        .sync_into(store, self.channel_filter().as_deref(), device.as_deref())
                        .await
                    {
                        // Poked only when something actually landed: a reader woken
                        // four times a second by an idle holder is worse than one
                        // that waits for its tick.
                        Ok(outcome) => self.wrote = !outcome.is_quiet(),
                        Err(error) => self.status = format!("sync: {error}"),
                    }
                }
                Err(error) => self.status = format!("store: {error}"),
            }
        }

        match session.inbox(self.channel_filter().as_deref(), 100).await {
            Ok(inbox) => {
                let fresh: Vec<Row> = inbox
                    .iter()
                    .map(|entry| {
                        // The arrival time is the store's to answer, and a row it has
                        // never seen simply has no badge.
                        let late = store
                            .delivered_at(&entry.id.to_string())
                            .ok()
                            .flatten()
                            .map(|delivered| delivered - entry.created_at)
                            .filter(|late| *late > TimeDelta::zero());
                        Row::from_inbox(entry, late)
                    })
                    .collect();
                // Through the same diff as the cache path: the holder reads the
                // inbox from the server, so an arrival that lands on a refresh has
                // to be noticed here too — and the first refresh is what marks the
                // first load, so the first poke after it is not swallowed.
                self.note_arrivals(&fresh);
                self.inbox = fresh;
                self.status = format!("{} notifications", self.inbox.len());
            }
            Err(error) => self.status = format!("inbox: {error}"),
        }

        self.clamp_selection();
    }

    /// Redraw from the local cache, without a request (design §9.3bis).
    ///
    /// What a front end that lost the election does, and what a poke from the holder
    /// triggers. The channel list is left alone: it comes from the server and does
    /// not change because a notification landed.
    pub(crate) fn reload_from_store(&mut self, store: &Store) {
        match store.cached_inbox(100) {
            Ok(cached) => {
                let fresh: Vec<Row> = cached.into_iter().map(Row::from_cache).collect();
                self.note_arrivals(&fresh);
                self.inbox = fresh;
                self.status = format!("{} notifications (cache)", self.inbox.len());
            }
            Err(error) => self.status = format!("store: {error}"),
        }

        self.clamp_selection();
    }

    /// Remember what is in `fresh` and was not on screen before.
    ///
    /// Identity is the notification id, not the revision: a notification that is
    /// revised twenty times during an incident is one arrival, not twenty popups.
    /// The first load is not an arrival either — everything is new then, and a TUI
    /// that opens by popping the whole inbox is a TUI nobody opens twice.
    fn note_arrivals(&mut self, fresh: &[Row]) {
        // `loaded` and not `inbox.is_empty()`, which is what this said first and was
        // wrong in a way nobody would have reported: somebody whose inbox is empty
        // — a fresh enrolment, or a channel that just expired everything — would
        // never be told about their **first** notification, and only that one.
        if !self.loaded {
            self.loaded = true;
            return;
        }

        let known: std::collections::HashSet<&str> =
            self.inbox.iter().map(|row| row.id.as_str()).collect();
        self.arrivals.extend(
            fresh
                .iter()
                .filter(|row| !known.contains(row.id.as_str()))
                .cloned(),
        );
    }

    fn clamp_selection(&mut self) {
        self.selected_row = self.selected_row.min(self.inbox.len().saturating_sub(1));
    }

    /// Which channel the inbox is narrowed to, if any.
    fn channel_filter(&self) -> Option<String> {
        if self.focus == Pane::Channels || self.selected_channel > 0 {
            self.channels
                .get(self.selected_channel)
                .map(|channel| channel.id.as_str().to_owned())
        } else {
            None
        }
    }

    /// The row the detail pane is showing.
    pub(crate) fn current(&self) -> Option<&Row> {
        self.inbox.get(self.selected_row)
    }
}

/// Parse a `:` line with the *same* clap parser the shell uses (§9, §9.2).
///
/// This is the whole parity mechanism. The line is split with shell rules, given
/// the program name clap expects, and parsed — so a subcommand added to the CLI is
/// available here without a line of TUI code.
///
/// `Ok(None)` is an empty line, and `Err` carries clap's own message verbatim: it
/// words usage errors, `--help` and `--version` better than anything written here
/// would, and reusing it is the point.
fn parse_command(line: &str) -> Result<Option<crate::Cli>, String> {
    use clap::Parser as _;

    let words = shell_split(line);
    if words.is_empty() {
        return Ok(None);
    }

    let argv = std::iter::once("notify".to_owned()).chain(words);

    crate::Cli::try_parse_from(argv)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Parse a `:` line and **run** it, giving back what it printed (§9.2).
///
/// The command goes through [`crate::execute`] — the very function the shell calls —
/// so the TUI shows the shell's answer rather than a second rendering of it. That is
/// §9.3's rule applied to presentation: a command rendered twice would drift, and
/// the drift would be invisible until somebody compared two screens.
pub(crate) async fn run_command(line: &str, profile: &str, session: &Session) -> String {
    let parsed = match parse_command(line) {
        Ok(None) => return String::new(),
        Ok(Some(parsed)) => parsed,
        Err(message) => return message,
    };

    let Some(command) = parsed.command else {
        // `:` with only global flags. Nothing to run, and saying so beats a blank
        // popup that reads as a hang.
        return "aucune commande — tape `:help` ou `:ls`".to_owned();
    };

    // The line's own `--profile` and `--output` are honoured: `:ls --output json` is
    // the same request it would be at a shell. The session, however, is this window's
    // — opening a second one per command would mean a second credential load and a
    // second connection pool for a command that prints four lines.
    match crate::execute(session, profile, parsed.output, command).await {
        Ok(rendered) if rendered.is_empty() => "(rien)".to_owned(),
        Ok(rendered) => rendered,
        // Rendered, not swallowed: the popup is the only place a `:` command can
        // report a refusal, and the stable code travels in the message (§9.1).
        Err(error) => format!("erreur: {error}"),
    }
}

/// Split a command line the way a shell would, honouring quotes.
///
/// Written rather than pulled in: the rule needed here is "quotes group words",
/// and a title with a space in it is the only reason it matters.
fn shell_split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for character in line.chars() {
        match (character, quote) {
            (c, Some(open)) if c == open => quote = None,
            (c, None) if c == '"' || c == '\'' => quote = Some(c),
            (c, None) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (c, _) => current.push(c),
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn app() -> App {
        App::new("default".to_owned(), true)
    }

    #[test]
    fn the_command_line_swallows_keys_that_are_bindings_outside_it() {
        // Typing `q` into `:publish -t "quit"` must not quit.
        let mut app = app();
        app.command = Some(String::new());

        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::CommandChar('q')
        );
        assert_eq!(app.key(KeyCode::Esc, KeyModifiers::NONE), Message::Cancel);
    }

    #[test]
    fn the_ordinary_bindings_are_vim_like() {
        let app = app();

        assert_eq!(
            app.key(KeyCode::Char('j'), KeyModifiers::NONE),
            Message::Move(1)
        );
        assert_eq!(app.key(KeyCode::Up, KeyModifiers::NONE), Message::Move(-1));
        assert_eq!(app.key(KeyCode::Tab, KeyModifiers::NONE), Message::NextPane);
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
    }

    #[test]
    fn moving_stops_at_the_ends_rather_than_wrapping() {
        // Wrapping in an inbox means holding `k` at the top lands on the oldest
        // notification, which is never what somebody meant.
        let mut app = app();
        app.focus = Pane::Inbox;
        app.selected_row = 0;

        app.move_selection(-1);
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn the_command_mode_uses_the_shells_own_parser() {
        // The parity property of §9: this is the CLI's parser, so `--help` is the
        // CLI's help and an unknown flag is the CLI's error.
        let refused = parse_command("publish --nonsense").unwrap_err();

        assert!(
            refused.contains("nonsense") || refused.contains("unexpected"),
            "{refused}"
        );
    }

    #[test]
    fn a_recognised_command_comes_back_as_the_command_the_shell_would_run() {
        // Not a description of it: the same value `main` dispatches on, which is
        // what makes `:` run commands rather than recognise them.
        let parsed = parse_command(r#"publish -c ci-infra -t "rebuild ok" --status success"#)
            .unwrap()
            .expect("a line with words in it is a command");

        assert!(matches!(
            parsed.command,
            Some(crate::Command::Publish { .. })
        ));
    }

    #[test]
    fn the_lines_own_output_flag_is_honoured() {
        // `:ls --output json` has to mean at a `:` prompt what it means at a shell
        // one, or the parity of §9 stops at the parser.
        let parsed = parse_command("ls --output json").unwrap().unwrap();

        assert!(matches!(parsed.output, crate::OutputFormat::Json));
    }

    #[test]
    fn quotes_group_a_title_with_spaces_in_it() {
        assert_eq!(
            shell_split(r#"publish -t "etcd degraded" --status failure"#),
            ["publish", "-t", "etcd degraded", "--status", "failure"]
        );
    }

    #[test]
    fn an_empty_command_line_does_nothing() {
        assert!(parse_command("   ").unwrap().is_none());
    }
    #[test]
    fn tab_and_the_arrows_belong_to_the_command_line_while_it_is_open() {
        // Outside it, Tab moves focus and the arrows move the selection (§9.2);
        // inside it they have to mean completion and history, or the two features
        // are unreachable from the keyboard they were written for.
        let mut app = app();
        app.command = Some(String::new());

        assert_eq!(
            app.key(KeyCode::Tab, KeyModifiers::NONE),
            Message::CompleteCommand
        );
        assert_eq!(
            app.key(KeyCode::Up, KeyModifiers::NONE),
            Message::RecallCommand(-1)
        );
        assert_eq!(
            app.key(KeyCode::Down, KeyModifiers::NONE),
            Message::RecallCommand(1)
        );

        app.command = None;
        assert_eq!(app.key(KeyCode::Tab, KeyModifiers::NONE), Message::NextPane);
        assert_eq!(app.key(KeyCode::Up, KeyModifiers::NONE), Message::Move(-1));
    }
    fn row(id: &str) -> Row {
        Row {
            id: id.to_owned(),
            channel: "ci-infra".to_owned(),
            title: format!("about {id}"),
            body: None,
            seq: 1,
            last_seq: 1,
            state: State::Open,
            status: Status::Info,
            late_by: None,
            muted_by_dnd: false,
        }
    }

    #[test]
    fn the_first_load_is_not_a_pile_of_arrivals() {
        // A TUI that opens by popping the whole inbox is a TUI nobody opens twice
        // (§9.2, `--notify-send`).
        let mut app = app();

        app.note_arrivals(&[row("ntf_1"), row("ntf_2")]);

        assert!(app.take_arrivals().is_empty());
    }

    #[test]
    fn the_first_notification_of_an_empty_inbox_is_still_an_arrival() {
        // The bug this pins: "is the inbox empty" was standing in for "have we
        // loaded yet", so somebody with an empty inbox — a fresh enrolment, or a
        // channel that just expired everything — was never told about their first
        // notification, and only that one. Silent, and unreportable.
        let mut app = app();
        app.note_arrivals(&[]); // the first load, of nothing at all
        assert!(app.take_arrivals().is_empty());

        app.note_arrivals(&[row("ntf_1")]);

        let arrived: Vec<String> = app.take_arrivals().into_iter().map(|r| r.id).collect();
        assert_eq!(arrived, ["ntf_1"]);
    }

    #[test]
    fn only_what_was_not_there_before_counts_as_an_arrival() {
        let mut app = app();
        app.loaded = true;
        app.inbox = vec![row("ntf_1")];

        app.note_arrivals(&[row("ntf_2"), row("ntf_1")]);

        let arrived: Vec<String> = app.take_arrivals().into_iter().map(|r| r.id).collect();
        assert_eq!(arrived, ["ntf_2"]);
    }

    #[test]
    fn a_notification_revised_twenty_times_is_one_arrival() {
        // Identity is the notification id, not the revision: an incident that gets
        // amended all afternoon must not pop all afternoon (§2.2).
        let mut app = app();
        app.loaded = true;
        app.inbox = vec![row("ntf_1")];

        let mut revised = row("ntf_2");
        app.note_arrivals(&[revised.clone()]);
        app.inbox = vec![row("ntf_1"), revised.clone()];
        revised.seq = 2;
        revised.title = "amended".to_owned();
        app.note_arrivals(&[revised]);

        assert_eq!(app.take_arrivals().len(), 1);
    }

    #[test]
    fn draining_the_arrivals_leaves_nothing_behind() {
        // The loop drains on every pass; a second pass must not pop the same thing.
        let mut app = app();
        app.loaded = true;
        app.inbox = vec![row("ntf_1")];
        app.note_arrivals(&[row("ntf_1"), row("ntf_2")]);

        assert_eq!(app.take_arrivals().len(), 1);
        assert!(app.take_arrivals().is_empty());
    }
}
