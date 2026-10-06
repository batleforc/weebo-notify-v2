//! The state, the messages, and the only function allowed to change one into the
//! other (design §9.2).

use std::borrow::Cow;
use std::collections::BTreeMap;

use chrono::TimeDelta;
use crossterm::event::{KeyCode, KeyModifiers};
use notify_client_core::types::{
    Action, ActionKey, Channel, ChoiceOptionKey, Interaction, Notification, SilenceBody, State,
    Status,
};
use notify_client_core::{CachedNotification, Session, Store};

use super::compose::{self, ComposeActivation};
use super::history;
use super::menu::{self, MenuOutcome};
use super::sanitise;
use crate::{OutputFormat, publish};

/// One line of the inbox, from the server (stream holder) or the local store (election loser,
/// §9.3bis), so drawing needn't know which. Text is sanitised once, here (§2.3).
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: String,
    pub channel: String,
    pub title: String,
    pub body: Option<String>,
    pub seq: i32,
    /// The newest revision the source knows (equal to `seq` from the cache).
    pub last_seq: i32,
    pub state: State,
    pub status: Status,
    /// How long after creation this profile first saw it (§7.2); `None` if never recorded.
    pub late_by: Option<TimeDelta>,
    /// Whether the server said this one landed inside a quiet period (§7.4); never re-derived.
    pub muted_by_dnd: bool,
    /// Buttons, in display order (design §2.3).
    pub actions: Vec<Action>,
}

impl Row {
    /// From the server's answer, with the arrival time only the local store knows.
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
            // `GET /inbox` has no delivery opinion, so never known muted (the safe direction).
            muted_by_dnd: false,
            actions: head
                .content
                .actions
                .clone()
                .map(|actions| actions.0)
                .unwrap_or_default(),
        }
    }

    /// From the local cache, which is what a reader draws.
    fn from_cache(cached: CachedNotification) -> Self {
        let late_by = cached.late_by();
        // The cache holds the real `u32`; saturate rather than wrap negative.
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
            actions: cached.actions,
        }
    }
}

/// One selectable line of the actions list (§2.3, §9.2); a `choice` is flattened into its
/// options so the list stays one flat `j`/`k` walk.
#[derive(Debug, Clone)]
pub(crate) enum ActionItem {
    /// `view`, `ack`, `reply` or `silence` — triggered as it stands.
    Whole(Action),
    /// One option of a `choice` action.
    ChoiceOption {
        key: ActionKey,
        option: ChoiceOptionKey,
        label: String,
    },
}

impl ActionItem {
    /// Flatten a notification's actions into what the list draws and `j`/`k` walks.
    fn list(actions: &[Action]) -> Vec<Self> {
        actions
            .iter()
            .flat_map(|action| -> Vec<Self> {
                match action {
                    Action::Choice { key, options, .. } => options
                        .iter()
                        .map(|option| Self::ChoiceOption {
                            key: key.clone(),
                            option: option.key.clone(),
                            label: option.label.clone(),
                        })
                        .collect(),
                    other => vec![Self::Whole(other.clone())],
                }
            })
            .collect()
    }

    /// What the list shows.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Whole(
                Action::View { label, .. }
                | Action::Ack { label, .. }
                | Action::Reply { label, .. }
                | Action::Choice { label, .. }
                | Action::Silence { label, .. },
            )
            | Self::ChoiceOption { label, .. } => label,
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

    /// The exact reverse of [`Self::next`] (`shift-tab`).
    const fn previous(self) -> Self {
        match self {
            Self::Channels => Self::Detail,
            Self::Inbox => Self::Channels,
            Self::Detail => Self::Inbox,
        }
    }
}

/// One entry of the contextual footer/help bar: a key and what it currently does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hint {
    pub key: &'static str,
    pub label: Cow<'static, str>,
}

impl Hint {
    fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label: Cow::Borrowed(label),
        }
    }

    fn owned(key: &'static str, label: String) -> Self {
        Self {
            key,
            label: Cow::Owned(label),
        }
    }
}

/// Everything that can change the state; `update` is the only place that reacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    /// Nothing happened; advance the clock and poll the store.
    Tick,
    /// The stream holder wrote to the store (§9.3bis): reload from it, not the server.
    StoreChanged,
    /// Move the selection in the focused pane.
    Move(i32),
    /// Move focus forward.
    NextPane,
    /// Move focus backward (`shift-tab`).
    PrevPane,
    /// Confirm the highlighted Channels row (index `0` is "Tous") as the inbox filter, and jump
    /// to the Inbox; see [`App::applied_channel`].
    SelectChannel,
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
    /// Open the current notification's action list (design §2.3).
    EnterActions,
    /// Trigger the highlighted action.
    TriggerAction,
    /// Open the guided menu for channel/dnd/device management (§9.2).
    OpenMenu,
    /// Go back one level in the menu, or close it from the root.
    MenuBack,
    /// Act on the menu's highlighted item.
    MenuSelect,
    /// Open the guided compose form (§9.2).
    OpenCompose,
    /// Step a cyclable field (channel, état, statut, priorité).
    ComposeCycle(i32),
    /// Act on the focused field — see [`ComposeActivation`].
    ComposeActivate,
    /// A typed character, while editing a text field of the form.
    ComposeChar(char),
    /// Backspace while editing a text field of the form.
    ComposeBackspace,
    /// Submit the `:` line as a `reply`, not a command (see [`App::reply_for`]).
    SubmitReply,
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
    /// Whether this front end holds the stream or reads what another writes (§9.3bis); shown.
    pub holds_stream: bool,
    pub channels: Vec<Channel>,
    /// Unread per channel id (§7.3, §9.2), the sidebar badge; an absent key means zero.
    pub unread: BTreeMap<String, u64>,
    pub inbox: Vec<Row>,
    pub focus: Pane,
    /// Cursor in the Channels pane: `0` is "Tous", `i > 0` is `channels[i - 1]`. Moving it
    /// doesn't filter (see [`Self::applied_channel`]).
    pub selected_channel: usize,
    /// The filter applied to the Inbox, indexed like [`Self::selected_channel`] but changed only
    /// by [`Message::SelectChannel`], so the active filter never depends on the cursor or focus.
    pub applied_channel: usize,
    pub selected_row: usize,
    /// How far the Détail pane's body is scrolled, in lines; reset when the selection changes.
    pub detail_scroll: u16,
    /// The highlighted action while the Détail action list is open; `None` when closed.
    pub selected_action: Option<usize>,
    /// The guided channel/dnd/device menu (§9.2), when open (at most one modal is `Some`).
    pub menu: Option<menu::Menu>,
    /// The guided compose form (§9.2), when open.
    pub compose: Option<compose::ComposeForm>,
    /// The notification a `reply` action answers while the `:` line is its text box; only what
    /// `Enter` does differs (see [`Message::SubmitReply`]).
    pub reply_for: Option<String>,
    /// The `:` line, when it is open.
    pub command: Option<String>,
    /// What the `:` line remembers between runs (§9.2), per profile.
    pub history: history::History,
    /// What the last command printed, shown in a popup.
    pub output: Option<String>,
    pub show_help: bool,
    pub status: String,
    pub should_quit: bool,
    /// Whether the inbox has ever been filled; the first load is not arrivals (see [`Self::note_arrivals`]).
    pub loaded: bool,
    /// Rows that appeared since the loop last looked, oldest first; drained by the loop (I/O stays
    /// out of `update`).
    pub arrivals: Vec<Row>,
    /// Set when this front end just wrote to the shared store; the loop pokes readers and clears it.
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
            applied_channel: 0,
            selected_row: 0,
            detail_scroll: 0,
            selected_action: None,
            menu: None,
            compose: None,
            reply_for: None,
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

    /// Turn a key into a message (testable without a terminal; the `:` line can swallow keys).
    pub(crate) fn key(&self, code: KeyCode, modifiers: KeyModifiers) -> Message {
        // `q` always quits, except during real text entry (the `:` line, a compose field mid-edit).
        let typing_text = self.command.is_some()
            || self
                .compose
                .as_ref()
                .is_some_and(compose::ComposeForm::is_editing);
        if !typing_text && code == KeyCode::Char('q') {
            return Message::Quit;
        }

        if self.command.is_some() {
            return match code {
                KeyCode::Esc => Message::Cancel,
                // The line means two things depending on why it is open (`App::reply_for`).
                KeyCode::Enter if self.reply_for.is_some() => Message::SubmitReply,
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
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        // The open action list swallows everything but `j`/`k`, `Enter` and `Esc`.
        if self.selected_action.is_some() {
            return match code {
                KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
                KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
                KeyCode::Enter => Message::TriggerAction,
                KeyCode::Esc => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        // The menu likewise; its `Esc` goes back one level, hence its own message.
        if self.menu.is_some() {
            return match code {
                KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
                KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
                KeyCode::Enter => Message::MenuSelect,
                KeyCode::Esc => Message::MenuBack,
                _ => Message::Ignored,
            };
        }

        // The compose form has two sub-modes: while typing, every character goes in the field; else
        // `j`/`k` move focus and `h`/`l` cycle a value.
        if let Some(form) = &self.compose {
            if form.is_editing() {
                return match code {
                    KeyCode::Enter | KeyCode::Esc => Message::ComposeActivate,
                    KeyCode::Backspace => Message::ComposeBackspace,
                    KeyCode::Char(character) => Message::ComposeChar(character),
                    _ => Message::Ignored,
                };
            }
            return match code {
                KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
                KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
                KeyCode::Char('h') | KeyCode::Left => Message::ComposeCycle(-1),
                KeyCode::Char('l') | KeyCode::Right => Message::ComposeCycle(1),
                KeyCode::Enter => Message::ComposeActivate,
                // Discards the whole form (no "one level up" here).
                KeyCode::Esc => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Message::Quit,
            KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
            KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
            KeyCode::Char('m') => Message::OpenMenu,
            KeyCode::Char('p') => Message::OpenCompose,
            // Some terminals report shift-tab as `Tab` + SHIFT rather than `BackTab`.
            KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => Message::PrevPane,
            KeyCode::Tab => Message::NextPane,
            KeyCode::BackTab => Message::PrevPane,
            // Enter opens: on Channels it confirms the filter (§9.2), on the Inbox it moves to Détail.
            KeyCode::Enter => match self.focus {
                Pane::Channels => Message::SelectChannel,
                Pane::Inbox => Message::NextPane,
                Pane::Detail => Message::Ignored,
            },
            KeyCode::Char('r') => Message::Refresh,
            KeyCode::Char(':') => Message::OpenCommand,
            KeyCode::Char('?') => Message::ToggleHelp,
            KeyCode::Char('a')
                if self.focus == Pane::Detail
                    && self.current().is_some_and(|row| !row.actions.is_empty()) =>
            {
                Message::EnterActions
            }
            _ => Message::Ignored,
        }
    }

    /// The only place state changes.
    pub(crate) async fn update(&mut self, message: Message, session: &Session, store: &Store) {
        match message {
            Message::Quit => self.should_quit = true,
            Message::NextPane => self.focus = self.focus.next(),
            Message::PrevPane => self.focus = self.focus.previous(),
            Message::SelectChannel => {
                self.select_channel();
                self.refresh(session, store).await;
            }
            Message::Move(delta) => self.move_selection(delta),
            Message::ToggleHelp => self.show_help = !self.show_help,
            Message::Cancel => self.close_modals(),
            Message::EnterActions => {
                if !self.action_items().is_empty() {
                    self.selected_action = Some(0);
                }
            }
            Message::TriggerAction => self.trigger_action(session, store).await,
            Message::OpenMenu => self.menu = Some(menu::Menu::root()),
            Message::MenuBack => {
                // `false` means only the root was left: the menu closes.
                if !self.menu.as_mut().is_some_and(menu::Menu::back) {
                    self.menu = None;
                }
            }
            Message::MenuSelect => self.handle_menu_select(session, store).await,
            Message::OpenCompose => self.open_compose(),
            Message::ComposeCycle(delta) => {
                if let Some(form) = &mut self.compose {
                    form.cycle(delta);
                }
            }
            Message::ComposeChar(character) => {
                if let Some(form) = &mut self.compose {
                    form.push_char(character);
                }
            }
            Message::ComposeBackspace => {
                if let Some(form) = &mut self.compose {
                    form.backspace();
                }
            }
            Message::ComposeActivate => self.handle_compose_activate(session, store).await,
            Message::SubmitReply => {
                let text = self.command.take().unwrap_or_default();
                let Some(id) = self.reply_for.take() else {
                    return;
                };
                self.history.push(&text);
                match session.interact(&id, Interaction::Reply { text }).await {
                    Ok(()) => "réponse envoyée".clone_into(&mut self.status),
                    Err(error) => self.status = format!("réponse: {error}"),
                }
                self.refresh(session, store).await;
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
                    // Past the newest is the line being typed: empty.
                    self.command = Some(recalled.unwrap_or_default());
                }
            }
            Message::RunCommand => {
                let line = self.command.take().unwrap_or_default();
                self.history.push(&line);
                self.output = Some(run_command(&line, &self.profile, session).await);
                // The server may now disagree with the screen.
                self.refresh(session, store).await;
            }
            Message::Refresh => self.refresh(session, store).await,
            // Reload from the store, not the server: that would spend the stream budget (§8.2, §9.3bis).
            Message::StoreChanged => self.reload_from_store(session, store).await,
            Message::Tick | Message::Ignored => {}
        }
    }

    /// Act on the guided menu's highlighted item (§9.2).
    async fn handle_menu_select(&mut self, session: &Session, store: &Store) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        match menu.activate() {
            MenuOutcome::Push | MenuOutcome::Nothing => {}
            MenuOutcome::RunLine(line) => {
                self.menu = None;
                self.output = Some(run_command(&line, &self.profile, session).await);
                self.refresh(session, store).await;
            }
            MenuOutcome::PrefillLine(line) => {
                self.menu = None;
                self.command = Some(line);
            }
        }
    }

    /// Open the compose form, or say why not.
    fn open_compose(&mut self) {
        if self.channels.is_empty() {
            // No channel to pick means nothing to submit.
            "crée ou rejoins un canal avant de publier (m → Canaux)".clone_into(&mut self.status);
        } else {
            self.compose = Some(compose::ComposeForm::new(
                self.channels.len(),
                self.applied_channel.saturating_sub(1),
            ));
        }
    }

    /// Act on the compose form's focused field (§9.2).
    async fn handle_compose_activate(&mut self, session: &Session, store: &Store) {
        let Some(outcome) = self.compose.as_mut().map(compose::ComposeForm::activate) else {
            return;
        };

        match outcome {
            ComposeActivation::Handled => {}
            ComposeActivation::Rejected(message) => self.status = message,
            ComposeActivation::Submit => {
                let Some(form) = self.compose.take() else {
                    return;
                };
                match form.to_content(&self.channels) {
                    Ok((channel, content)) => {
                        let result =
                            publish::publish(session, &channel, content, None, OutputFormat::Table)
                                .await;
                        self.output =
                            Some(result.unwrap_or_else(|error| format!("erreur: {error}")));
                        self.refresh(session, store).await;
                    }
                    Err(message) => {
                        self.status = message;
                        self.compose = Some(form);
                    }
                }
            }
        }
    }

    /// Close whatever popup, list or form is open ([`Message::Cancel`]).
    fn close_modals(&mut self) {
        self.command = None;
        self.reply_for = None;
        self.output = None;
        self.show_help = false;
        self.selected_action = None;
        self.menu = None;
        self.compose = None;
    }

    /// Confirm the highlighted Channels row as the applied filter and focus the Inbox.
    fn select_channel(&mut self) {
        self.applied_channel = self.selected_channel;
        self.focus = Pane::Inbox;
    }

    fn move_selection(&mut self, delta: i32) {
        // The compose form owns `j`/`k` when open (only reached outside edit mode).
        if let Some(form) = &mut self.compose {
            form.move_focus(delta);
            return;
        }

        // The menu owns `j`/`k` when open.
        if let Some(menu) = &mut self.menu {
            menu.move_cursor(delta);
            return;
        }

        // The open action list comes next, before the inbox selection and detail scroll.
        if let Some(selected) = self.selected_action {
            let len = self.action_items().len();
            if len == 0 {
                return;
            }
            self.selected_action = Some(if delta.is_negative() {
                selected.saturating_sub(delta.unsigned_abs() as usize)
            } else {
                (selected + delta.unsigned_abs() as usize).min(len - 1)
            });
            return;
        }

        // Détail focused: scroll the body instead of moving the inbox selection (§9.2). The upper
        // bound is clamped when drawn (`render::detail`).
        if self.focus == Pane::Detail {
            let magnitude = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
            self.detail_scroll = if delta.is_negative() {
                self.detail_scroll.saturating_sub(magnitude)
            } else {
                self.detail_scroll.saturating_add(magnitude)
            };
            return;
        }

        let (index, len) = match self.focus {
            // `+ 1` for the "Tous" row; never touches `applied_channel`.
            Pane::Channels => (&mut self.selected_channel, self.channels.len() + 1),
            Pane::Inbox => (&mut self.selected_row, self.inbox.len()),
            Pane::Detail => unreachable!("handled above"),
        };

        if len == 0 {
            return;
        }

        let previous = *index;
        // Saturating: a wrapped selection would silently paint an empty detail pane.
        *index = if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (*index + delta.unsigned_abs() as usize).min(len - 1)
        };

        // A different notification: reset the scroll.
        if self.focus == Pane::Inbox && *index != previous {
            self.detail_scroll = 0;
        }
    }

    /// Ask the server for both lists, and feed the store if this front end holds it.
    pub(crate) async fn refresh(&mut self, session: &Session, store: &Store) {
        match session.channels().await {
            Ok(channels) => self.channels = channels,
            Err(error) => self.status = format!("canaux: {error}"),
        }

        self.refresh_unread(session).await;

        // Only the holder writes (§9.3bis), before reading so a catch-up shows this frame.
        if self.holds_stream {
            match store.device() {
                Ok(device) => {
                    match session
                        .sync_into(store, self.channel_filter().as_deref(), device.as_deref())
                        .await
                    {
                        // Poke only when something landed.
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
                        let late = store
                            .delivered_at(&entry.id.to_string())
                            .ok()
                            .flatten()
                            .map(|delivered| delivered - entry.created_at)
                            .filter(|late| *late > TimeDelta::zero());
                        Row::from_inbox(entry, late)
                    })
                    .collect();
                // Same arrival diff as the cache path; the first refresh marks the first load.
                self.note_arrivals(&fresh);
                self.inbox = fresh;
                self.status = format!("{} notifications", self.inbox.len());
            }
            Err(error) => self.status = format!("inbox: {error}"),
        }

        self.clamp_selection();
    }

    /// Do whatever the highlighted action means (§2.3). `view` is client-side, `reply` goes out on
    /// [`Message::SubmitReply`]; anything else is one call, then the list closes.
    async fn trigger_action(&mut self, session: &Session, store: &Store) {
        let Some(id) = self.current().map(|row| row.id.clone()) else {
            return;
        };
        let Some(item) = self
            .selected_action
            .and_then(|selected| self.action_items().into_iter().nth(selected))
        else {
            return;
        };

        let outcome = match item {
            ActionItem::Whole(Action::View { url, .. }) => {
                self.selected_action = None;
                match open::that(&url) {
                    Ok(()) => Ok(format!("ouvert : {url}")),
                    Err(error) => Err(format!("impossible d'ouvrir {url}: {error}")),
                }
            }
            ActionItem::Whole(Action::Reply { .. }) => {
                // The `:` line becomes the text box; `Message::SubmitReply` sends it.
                self.selected_action = None;
                self.command = Some(String::new());
                self.reply_for = Some(id);
                return;
            }
            ActionItem::Whole(Action::Ack { key, .. }) => {
                self.selected_action = None;
                session
                    .interact(&id, Interaction::Ack { key })
                    .await
                    .map(|()| "reçu".to_owned())
                    .map_err(|error| error.to_string())
            }
            ActionItem::Whole(Action::Silence {
                scope, duration, ..
            }) => {
                self.selected_action = None;
                session
                    .silence(
                        &id,
                        SilenceBody {
                            scope: Some(scope),
                            duration,
                        },
                    )
                    .await
                    .map(|()| "mis en silence".to_owned())
                    .map_err(|error| error.to_string())
            }
            ActionItem::Whole(Action::Choice { .. }) => {
                // `ActionItem::list` expands a `choice` into its options, so this never arrives.
                self.selected_action = None;
                return;
            }
            ActionItem::ChoiceOption { key, option, .. } => {
                self.selected_action = None;
                session
                    .interact(&id, Interaction::Choice { key, option })
                    .await
                    .map(|()| "choix enregistré".to_owned())
                    .map_err(|error| error.to_string())
            }
        };

        self.status = outcome.unwrap_or_else(|message| message);
        self.refresh(session, store).await;
    }

    /// Ask the server for unread counts per channel (server-authoritative, §7.3).
    async fn refresh_unread(&mut self, session: &Session) {
        // A failed badge load leaves no badge rather than failing the frame.
        match session.unread().await {
            Ok(counts) => {
                // The generated client widens the count to `i64`; drop anything that isn't a positive count.
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
    }

    /// Redraw the inbox from the local cache (§9.3bis), as election losers and pokes do. Unread
    /// counts are still asked of the server, once per poke, as the web client does.
    pub(crate) async fn reload_from_store(&mut self, session: &Session, store: &Store) {
        match store.cached_inbox(100) {
            Ok(cached) => {
                let fresh: Vec<Row> = cached.into_iter().map(Row::from_cache).collect();
                self.note_arrivals(&fresh);
                self.inbox = fresh;
                self.status = format!("{} notifications (cache)", self.inbox.len());
            }
            Err(error) => self.status = format!("store: {error}"),
        }

        self.refresh_unread(session).await;

        self.clamp_selection();
    }

    /// Remember rows in `fresh` not on screen before, by notification id (not revision). The
    /// first load is not an arrival.
    fn note_arrivals(&mut self, fresh: &[Row]) {
        // `loaded`, not `inbox.is_empty()`: an empty inbox must still announce its first notification.
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
        // Index `channels.len()` is the last real channel, after "Tous" at 0.
        self.selected_channel = self.selected_channel.min(self.channels.len());
        self.applied_channel = self.applied_channel.min(self.channels.len());
    }

    /// Which channel the inbox is narrowed to: [`Self::applied_channel`] alone.
    fn channel_filter(&self) -> Option<String> {
        self.applied_channel
            .checked_sub(1)
            .and_then(|index| self.channels.get(index))
            .map(|channel| channel.id.as_str().to_owned())
    }

    /// What the Inbox/Détail titles show, so the active filter is always visible.
    pub(crate) fn filter_label(&self) -> &str {
        self.applied_channel
            .checked_sub(1)
            .and_then(|index| self.channels.get(index))
            .map_or("tous canaux", |channel| channel.id.as_str())
    }

    /// The row the detail pane is showing.
    pub(crate) fn current(&self) -> Option<&Row> {
        self.inbox.get(self.selected_row)
    }

    /// The current notification's actions, flattened for the list (§2.3); empty if none.
    pub(crate) fn action_items(&self) -> Vec<ActionItem> {
        self.current()
            .map(|row| ActionItem::list(&row.actions))
            .unwrap_or_default()
    }

    /// The bindings that mean something right now, for the footer hint row; walks the same
    /// priority tiers as [`Self::key`] so the two agree.
    pub(crate) fn footer_hints(&self) -> Vec<Hint> {
        if self.command.is_some() {
            return if self.reply_for.is_some() {
                vec![Hint::new("enter", "envoyer"), Hint::new("esc", "annuler")]
            } else {
                vec![
                    Hint::new("tab", "compléter"),
                    Hint::new("↑↓", "historique"),
                    Hint::new("enter", "exécuter"),
                    Hint::new("esc", "annuler"),
                ]
            };
        }

        if self.output.is_some() || self.show_help {
            return vec![Hint::new("esc/enter", "fermer"), Hint::new("q", "quitter")];
        }

        if self.selected_action.is_some() {
            return vec![
                Hint::new("j/k", "naviguer"),
                Hint::new("enter", "choisir"),
                Hint::new("esc", "annuler"),
                Hint::new("q", "quitter"),
            ];
        }

        if self.menu.is_some() {
            return vec![
                Hint::new("j/k", "naviguer"),
                Hint::new("enter", "choisir"),
                Hint::new("esc", "retour"),
                Hint::new("q", "quitter"),
            ];
        }

        if let Some(form) = &self.compose {
            return if form.is_editing() {
                vec![Hint::new("enter/esc", "valider le champ")]
            } else {
                vec![
                    Hint::new("j/k", "champ"),
                    Hint::new("h/l", "valeur"),
                    Hint::new("enter", "modifier / choisir"),
                    Hint::new("esc", "annuler"),
                    Hint::new("q", "quitter"),
                ]
            };
        }

        let mut hints = vec![
            Hint::new("j/k", "naviguer"),
            Hint::new("tab/⇧tab", "panneau"),
        ];
        match self.focus {
            Pane::Channels => hints.push(Hint::new("enter", "filtrer")),
            Pane::Inbox => hints.push(Hint::new("enter", "détail")),
            Pane::Detail => {
                let count = self.action_items().len();
                if count > 0 {
                    hints.push(Hint::owned("a", format!("{count} actions")));
                }
            }
        }
        hints.extend([
            Hint::new("p", "publier"),
            Hint::new("m", "menu"),
            Hint::new("r", "rafraîchir"),
            Hint::new(":", "commande"),
            Hint::new("?", "aide"),
            Hint::new("q", "quitter"),
        ]);
        hints
    }

    /// The full keybinding reference for the `?` popup, grouped, in [`Self::footer_hints`]'s wording.
    pub(crate) fn help_sections() -> Vec<(&'static str, Vec<Hint>)> {
        vec![
            (
                "Global",
                vec![
                    Hint::new("j/k", "naviguer / défiler"),
                    Hint::new("tab/⇧tab", "panneau (avant / arrière)"),
                    Hint::new("p", "publier une notification"),
                    Hint::new("m", "menu canaux / dnd / appareils"),
                    Hint::new("r", "rafraîchir"),
                    Hint::new(":", "commande"),
                    Hint::new("?", "aide"),
                    Hint::new("q", "quitter"),
                ],
            ),
            ("Canaux", vec![Hint::new("enter", "filtrer sur ce canal")]),
            ("Inbox", vec![Hint::new("enter", "ouvrir le détail")]),
            ("Détail", vec![Hint::new("a", "actions de la notification")]),
            (
                "Liste d'actions",
                vec![
                    Hint::new("j/k", "naviguer"),
                    Hint::new("enter", "choisir"),
                    Hint::new("esc", "annuler"),
                ],
            ),
            (
                "Menu",
                vec![
                    Hint::new("j/k", "naviguer"),
                    Hint::new("enter", "choisir"),
                    Hint::new("esc", "retour"),
                ],
            ),
            (
                "Composer",
                vec![
                    Hint::new("j/k", "changer de champ"),
                    Hint::new("h/l", "changer la valeur (canal, état, statut, priorité)"),
                    Hint::new("enter", "modifier un champ texte, ou publier"),
                    Hint::new("esc", "valider le champ, ou annuler le formulaire"),
                ],
            ),
            (
                "Ligne `:`",
                vec![
                    Hint::new("tab", "compléter"),
                    Hint::new("↑↓", "historique"),
                    Hint::new("enter", "exécuter"),
                    Hint::new("esc", "annuler"),
                ],
            ),
        ]
    }
}

/// Parse a `:` line with the shell's own clap parser (§9.2), split with shell rules.
/// `Ok(None)` is an empty line; `Err` is clap's message verbatim.
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

/// Parse a `:` line and run it through [`crate::execute`], returning what the shell would print (§9.3).
pub(crate) async fn run_command(line: &str, profile: &str, session: &Session) -> String {
    let parsed = match parse_command(line) {
        Ok(None) => return String::new(),
        Ok(Some(parsed)) => parsed,
        Err(message) => return message,
    };

    let Some(command) = parsed.command else {
        // Only global flags: say so rather than show a blank popup.
        return "aucune commande — tape `:help` ou `:ls`".to_owned();
    };

    // The line's `--profile`/`--output` are honoured, but this window's session is reused.
    match crate::execute(session, profile, parsed.output, command).await {
        Ok(rendered) if rendered.is_empty() => "(rien)".to_owned(),
        Ok(rendered) => rendered,
        // The popup is the only place to report a refusal; the stable code is in the message (§9.1).
        Err(error) => format!("erreur: {error}"),
    }
}

/// Split a command line the way a shell would, honouring quotes.
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

    use notify_client_core::types::ChoiceOption;

    use super::*;

    fn app() -> App {
        App::new("default".to_owned(), true)
    }

    /// A minimal `Channel`, from JSON since most of its field types aren't re-exported.
    fn channel(id: &str) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "origin": "dynamic",
            "members": [],
            "group_access": [],
            "policy": {
                "close_after": "7d",
                "default_priority": "default",
                "receipts_enabled": false,
                "ttl": { "allow_forever": false, "default": "7d" },
            },
        }))
        .expect("a fixture channel must deserialise")
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
    fn shift_tab_moves_focus_the_other_way_from_tab() {
        let app = app();

        // `BackTab` is what most terminals report for shift-tab.
        assert_eq!(
            app.key(KeyCode::BackTab, KeyModifiers::NONE),
            Message::PrevPane
        );
        // Some report `Tab` with the shift modifier set instead.
        assert_eq!(
            app.key(KeyCode::Tab, KeyModifiers::SHIFT),
            Message::PrevPane
        );
        // Plain `Tab` is unaffected.
        assert_eq!(app.key(KeyCode::Tab, KeyModifiers::NONE), Message::NextPane);
    }

    #[test]
    fn previous_pane_is_the_exact_reverse_of_next_pane() {
        for pane in [Pane::Channels, Pane::Inbox, Pane::Detail] {
            assert_eq!(pane.next().previous(), pane);
            assert_eq!(pane.previous().next(), pane);
        }
    }

    fn ack_action() -> Action {
        Action::Ack {
            key: ActionKey::from("ack-etcd".to_owned()),
            label: "Je m'en occupe".to_owned(),
        }
    }

    #[test]
    fn a_pressed_only_opens_the_action_list_when_detail_is_focused_and_there_are_any() {
        let mut app = app();
        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action()];
        app.inbox = vec![with_actions, row("ntf_2")];

        // Inbox focused: `a` is not the actions binding here, whatever the row has.
        app.focus = Pane::Inbox;
        assert_eq!(
            app.key(KeyCode::Char('a'), KeyModifiers::NONE),
            Message::Ignored
        );

        // Détail focused, but on the row with no actions.
        app.focus = Pane::Detail;
        app.selected_row = 1;
        assert_eq!(
            app.key(KeyCode::Char('a'), KeyModifiers::NONE),
            Message::Ignored
        );

        // Détail focused, on the row that has one.
        app.selected_row = 0;
        assert_eq!(
            app.key(KeyCode::Char('a'), KeyModifiers::NONE),
            Message::EnterActions
        );
    }

    #[test]
    fn moving_in_the_action_list_does_not_scroll_or_move_the_inbox() {
        let mut app = app();
        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action(), ack_action()];
        app.inbox = vec![with_actions];
        app.focus = Pane::Detail;
        app.selected_action = Some(0);

        app.move_selection(1);

        assert_eq!(app.selected_action, Some(1));
        assert_eq!(
            app.selected_row, 0,
            "the inbox selection must not have moved"
        );
        assert_eq!(app.detail_scroll, 0, "the body must not have scrolled");

        // Stops at the end rather than wrapping, same rule as everywhere else.
        app.move_selection(5);
        assert_eq!(app.selected_action, Some(1));
    }

    #[test]
    fn escaping_the_action_list_returns_to_ordinary_detail_scrolling() {
        let mut app = app();
        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action()];
        app.inbox = vec![with_actions];
        app.focus = Pane::Detail;
        app.selected_action = Some(0);

        assert_eq!(app.key(KeyCode::Esc, KeyModifiers::NONE), Message::Cancel);

        app.selected_action = None; // what `update` would have done with `Cancel`
        app.move_selection(1);
        assert_eq!(
            app.detail_scroll, 1,
            "j/k must scroll the body again once the list is closed"
        );
    }

    #[test]
    fn a_choice_action_flattens_into_one_item_per_option() {
        let choice = Action::Choice {
            key: ActionKey::from("escalate".to_owned()),
            label: "Escalade".to_owned(),
            options: vec![
                ChoiceOption {
                    key: ChoiceOptionKey::from("oncall".to_owned()),
                    label: "Page on-call".to_owned(),
                },
                ChoiceOption {
                    key: ChoiceOptionKey::from("wait".to_owned()),
                    label: "Attendre".to_owned(),
                },
            ],
        };

        let items = ActionItem::list(&[ack_action(), choice]);

        assert_eq!(items.len(), 3, "one ack, plus one item per choice option");
        assert_eq!(items[0].label(), "Je m'en occupe");
        assert_eq!(items[1].label(), "Page on-call");
        assert_eq!(items[2].label(), "Attendre");
        assert!(
            matches!(&items[1], ActionItem::ChoiceOption { key, .. } if key.as_str() == "escalate"),
            "every option carries the parent action's key, not one of its own"
        );
    }

    #[test]
    fn moving_with_detail_focused_scrolls_the_body_instead_of_the_selection() {
        // `j`/`k` on Détail must scroll, not jump to the next notification.
        let mut app = app();
        app.inbox = vec![row("ntf_1"), row("ntf_2")];
        app.focus = Pane::Detail;
        app.selected_row = 0;

        app.move_selection(1);
        app.move_selection(1);

        assert_eq!(app.selected_row, 0, "the selection must not have moved");
        assert_eq!(app.detail_scroll, 2);

        app.move_selection(-1);
        assert_eq!(app.detail_scroll, 1);
    }

    #[test]
    fn detail_scroll_does_not_go_negative() {
        let mut app = app();
        app.inbox = vec![row("ntf_1")];
        app.focus = Pane::Detail;

        app.move_selection(-5);

        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn selecting_a_different_row_resets_the_detail_scroll() {
        let mut app = app();
        app.inbox = vec![row("ntf_1"), row("ntf_2")];
        app.focus = Pane::Detail;
        app.detail_scroll = 7;

        app.focus = Pane::Inbox;
        app.move_selection(1);

        assert_eq!(app.selected_row, 1);
        assert_eq!(
            app.detail_scroll, 0,
            "a different notification is on screen now, not the one that was scrolled into"
        );
    }

    #[test]
    fn moving_stops_at_the_ends_rather_than_wrapping() {
        let mut app = app();
        app.focus = Pane::Inbox;
        app.selected_row = 0;

        app.move_selection(-1);
        assert_eq!(app.selected_row, 0);
    }

    #[test]
    fn the_command_mode_uses_the_shells_own_parser() {
        // The parity property of §9: the CLI's own parser and errors.
        let refused = parse_command("publish --nonsense").unwrap_err();

        assert!(
            refused.contains("nonsense") || refused.contains("unexpected"),
            "{refused}"
        );
    }

    #[test]
    fn a_recognised_command_comes_back_as_the_command_the_shell_would_run() {
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
        // Inside the `:` line, Tab and arrows mean completion and history.
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
            actions: Vec::new(),
        }
    }

    #[test]
    fn the_first_load_is_not_a_pile_of_arrivals() {
        let mut app = app();

        app.note_arrivals(&[row("ntf_1"), row("ntf_2")]);

        assert!(app.take_arrivals().is_empty());
    }

    #[test]
    fn the_first_notification_of_an_empty_inbox_is_still_an_arrival() {
        // An empty inbox must still announce its first notification.
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
        // One arrival per notification id, however often it's revised (§2.2).
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

    #[test]
    fn the_applied_filter_does_not_follow_focus_or_the_cursor() {
        // Focus and cursor must not filter; only a confirmed `SelectChannel` does.
        let mut app = app();
        app.channels = vec![channel("ci-infra"), channel("cluster-ovh")];
        app.focus = Pane::Detail;
        app.selected_channel = 2;
        app.applied_channel = 0;

        assert_eq!(app.channel_filter(), None);
    }

    #[test]
    fn moving_the_channels_cursor_does_not_change_the_filter() {
        let mut app = app();
        app.channels = vec![channel("ci-infra"), channel("cluster-ovh")];
        app.focus = Pane::Channels;
        app.applied_channel = 0;

        app.move_selection(1);
        app.move_selection(1);

        assert_eq!(app.selected_channel, 2);
        assert_eq!(
            app.channel_filter(),
            None,
            "the cursor moved, not the filter"
        );
    }

    #[test]
    fn the_channels_cursor_can_reach_every_real_channel_past_the_synthetic_tous_row() {
        let mut app = app();
        app.channels = vec![channel("ci-infra"), channel("cluster-ovh")];
        app.focus = Pane::Channels;

        app.move_selection(5); // clamps rather than overshoots
        assert_eq!(app.selected_channel, 2, "index 2 is the last real channel");
    }

    #[test]
    fn confirming_a_channel_applies_it_and_moves_focus_to_inbox() {
        let mut app = app();
        app.channels = vec![channel("ci-infra"), channel("cluster-ovh")];
        app.focus = Pane::Channels;
        app.selected_channel = 2;

        app.select_channel();

        assert_eq!(app.applied_channel, 2);
        assert_eq!(app.channel_filter().as_deref(), Some("cluster-ovh"));
        assert_eq!(app.focus, Pane::Inbox);
    }

    #[test]
    fn filter_label_names_the_applied_channel_or_says_tous() {
        let mut app = app();
        app.channels = vec![channel("ci-infra"), channel("cluster-ovh")];

        assert_eq!(app.filter_label(), "tous canaux");

        app.applied_channel = 1;
        assert_eq!(app.filter_label(), "ci-infra");
    }

    #[test]
    fn clamp_selection_keeps_channel_indices_in_range_after_the_list_shrinks() {
        let mut app = app();
        app.channels = vec![channel("a"), channel("b"), channel("c")];
        app.selected_channel = 3;
        app.applied_channel = 3;

        app.channels = vec![channel("a")];
        app.clamp_selection();

        assert_eq!(app.selected_channel, 1);
        assert_eq!(app.applied_channel, 1);
    }

    #[test]
    fn enter_means_something_different_per_pane() {
        let mut app = app();
        app.channels = vec![channel("ci-infra")];

        app.focus = Pane::Channels;
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::SelectChannel
        );

        app.focus = Pane::Inbox;
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::NextPane
        );

        app.focus = Pane::Detail;
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::Ignored,
            "nothing left to open; Tab still cycles all the way round"
        );
    }

    #[test]
    fn footer_hints_for_the_command_line_match_reply_vs_shell_mode() {
        let mut app = app();
        app.command = Some(String::new());

        let hints = app.footer_hints();
        assert!(hints.iter().any(|hint| hint.key == "tab"));

        app.reply_for = Some("ntf_1".to_owned());
        let hints = app.footer_hints();
        assert!(
            hints
                .iter()
                .any(|hint| hint.key == "enter" && hint.label == "envoyer")
        );
        assert!(
            !hints.iter().any(|hint| hint.key == "tab"),
            "a reply is not a shell command — nothing to complete"
        );
    }

    #[test]
    fn footer_hints_for_a_popup_offer_closing_it_and_quitting_separately() {
        let mut app = app();
        app.show_help = true;
        assert_eq!(
            app.footer_hints(),
            vec![Hint::new("esc/enter", "fermer"), Hint::new("q", "quitter")]
        );
    }

    #[test]
    fn footer_hints_for_the_action_list_are_navigation_plus_quit() {
        let mut app = app();
        app.selected_action = Some(0);
        assert_eq!(
            app.footer_hints(),
            vec![
                Hint::new("j/k", "naviguer"),
                Hint::new("enter", "choisir"),
                Hint::new("esc", "annuler"),
                Hint::new("q", "quitter"),
            ]
        );
    }

    #[test]
    fn footer_hints_for_the_menu_are_navigation_plus_quit() {
        let mut app = app();
        app.menu = Some(menu::Menu::root());
        assert_eq!(
            app.footer_hints(),
            vec![
                Hint::new("j/k", "naviguer"),
                Hint::new("enter", "choisir"),
                Hint::new("esc", "retour"),
                Hint::new("q", "quitter"),
            ]
        );
    }

    #[test]
    fn footer_hints_for_the_compose_form_differ_while_editing() {
        let mut app = app();
        app.channels = vec![channel("ci-infra")];
        app.compose = Some(compose::ComposeForm::new(1, 0));

        let hints = app.footer_hints();
        assert!(hints.iter().any(|hint| hint.key == "h/l"));

        let mut editing = compose::ComposeForm::new(1, 0);
        editing.move_focus(1); // Title
        editing.activate(); // start editing
        app.compose = Some(editing);
        let hints = app.footer_hints();
        assert!(
            !hints.iter().any(|hint| hint.key == "h/l"),
            "there is nothing to cycle while typing"
        );
    }

    #[test]
    fn footer_hints_show_a_live_action_count_only_when_there_is_one() {
        let mut app = app();
        app.focus = Pane::Detail;
        app.inbox = vec![row("ntf_1")];
        assert!(
            !app.footer_hints().iter().any(|hint| hint.key == "a"),
            "no actions, no hint — a static, always-shown `a` binding is the bug this fixes"
        );

        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action(), ack_action()];
        app.inbox = vec![with_actions];
        let hints = app.footer_hints();
        let action_hint = hints
            .iter()
            .find(|hint| hint.key == "a")
            .expect("a hint for `a` once the row has actions");
        assert_eq!(action_hint.label, "2 actions");
    }

    #[test]
    fn every_single_key_ordinary_hint_actually_does_something() {
        // Drift guard: every footer hint must be a key `key()` handles.
        fn single_key(key: &str) -> Option<KeyCode> {
            match key {
                "tab" => Some(KeyCode::Tab),
                "enter" => Some(KeyCode::Enter),
                "r" => Some(KeyCode::Char('r')),
                ":" => Some(KeyCode::Char(':')),
                "?" => Some(KeyCode::Char('?')),
                "q" => Some(KeyCode::Char('q')),
                "a" => Some(KeyCode::Char('a')),
                "m" => Some(KeyCode::Char('m')),
                "p" => Some(KeyCode::Char('p')),
                _ => None,
            }
        }

        let mut app = app();
        app.focus = Pane::Detail;
        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action()];
        app.inbox = vec![with_actions];

        for hint in app.footer_hints() {
            let Some(code) = single_key(hint.key) else {
                continue;
            };
            assert_ne!(
                app.key(code, KeyModifiers::NONE),
                Message::Ignored,
                "hint `{}` claims to do something but key() ignores it",
                hint.key
            );
        }
    }

    #[test]
    fn help_sections_cover_every_pane_and_mode_named_in_the_bindings() {
        let sections: Vec<&str> = App::help_sections()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            sections,
            [
                "Global",
                "Canaux",
                "Inbox",
                "Détail",
                "Liste d'actions",
                "Menu",
                "Composer",
                "Ligne `:`"
            ]
        );
    }

    #[test]
    fn m_opens_the_guided_menu() {
        let app = app();
        assert_eq!(
            app.key(KeyCode::Char('m'), KeyModifiers::NONE),
            Message::OpenMenu
        );
    }

    #[test]
    fn the_menu_swallows_keys_that_are_bindings_outside_it() {
        let mut app = app();
        app.menu = Some(menu::Menu::root());

        assert_eq!(
            app.key(KeyCode::Char('j'), KeyModifiers::NONE),
            Message::Move(1)
        );
        assert_eq!(
            app.key(KeyCode::Char('k'), KeyModifiers::NONE),
            Message::Move(-1)
        );
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::MenuSelect
        );
        assert_eq!(app.key(KeyCode::Esc, KeyModifiers::NONE), Message::MenuBack);
    }

    #[test]
    fn moving_in_the_menu_does_not_move_the_channels_cursor() {
        let mut app = app();
        app.channels = vec![channel("ci-infra")];
        app.menu = Some(menu::Menu::root());

        app.move_selection(1);

        assert_eq!(
            app.selected_channel, 0,
            "the channels cursor must not have moved"
        );
    }

    #[test]
    fn p_opens_the_compose_form() {
        let app = app();
        assert_eq!(
            app.key(KeyCode::Char('p'), KeyModifiers::NONE),
            Message::OpenCompose
        );
    }

    #[test]
    fn the_compose_form_swallows_keys_that_are_bindings_outside_it() {
        let mut app = app();
        app.compose = Some(compose::ComposeForm::new(1, 0));

        assert_eq!(
            app.key(KeyCode::Char('j'), KeyModifiers::NONE),
            Message::Move(1)
        );
        assert_eq!(
            app.key(KeyCode::Char('h'), KeyModifiers::NONE),
            Message::ComposeCycle(-1)
        );
        assert_eq!(
            app.key(KeyCode::Char('l'), KeyModifiers::NONE),
            Message::ComposeCycle(1)
        );
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::ComposeActivate
        );
        assert_eq!(app.key(KeyCode::Esc, KeyModifiers::NONE), Message::Cancel);
    }

    #[test]
    fn typing_into_the_compose_form_does_not_move_focus_or_quit() {
        // Typing a title containing `j`/`k`/`q` must not move focus or quit.
        // focus or quit the TUI character by character.
        let mut app = app();
        let mut form = compose::ComposeForm::new(1, 0);
        form.move_focus(1); // Title
        form.activate(); // start editing
        app.compose = Some(form);

        assert_eq!(
            app.key(KeyCode::Char('j'), KeyModifiers::NONE),
            Message::ComposeChar('j')
        );
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::ComposeChar('q')
        );
        assert_eq!(
            app.key(KeyCode::Backspace, KeyModifiers::NONE),
            Message::ComposeBackspace
        );
        assert_eq!(
            app.key(KeyCode::Enter, KeyModifiers::NONE),
            Message::ComposeActivate,
            "enter leaves edit mode rather than typing a newline"
        );
    }

    #[test]
    fn moving_in_the_compose_form_does_not_move_the_channels_cursor() {
        let mut app = app();
        app.channels = vec![channel("ci-infra")];
        app.compose = Some(compose::ComposeForm::new(1, 0));

        app.move_selection(1);

        assert_eq!(
            app.selected_channel, 0,
            "the channels cursor must not have moved"
        );
    }

    #[test]
    fn q_quits_from_every_ordinary_and_modal_state() {
        // `q` quits from every modal, never merely closing it.
        let mut with_actions = row("ntf_1");
        with_actions.actions = vec![ack_action()];

        let mut app = app();
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );

        app.show_help = true;
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
        app.show_help = false;

        app.output = Some("sortie".to_owned());
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
        app.output = None;

        app.inbox = vec![with_actions];
        app.selected_action = Some(0);
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
        app.selected_action = None;

        app.menu = Some(menu::Menu::root());
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
        app.menu = None;

        app.compose = Some(compose::ComposeForm::new(1, 0));
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::Quit
        );
    }

    #[test]
    fn q_types_instead_of_quitting_while_actual_text_is_being_entered() {
        let mut app = app();

        app.command = Some(String::new());
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::CommandChar('q'),
            "the `:` line is prose, not bindings"
        );
        app.command = None;

        let mut form = compose::ComposeForm::new(1, 0);
        form.move_focus(1); // Title
        form.activate(); // start editing
        app.compose = Some(form);
        assert_eq!(
            app.key(KeyCode::Char('q'), KeyModifiers::NONE),
            Message::ComposeChar('q'),
            "a compose field mid-edit is prose too"
        );
    }
}
