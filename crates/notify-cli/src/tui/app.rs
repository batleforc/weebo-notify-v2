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
    /// Buttons, in display order (design §2.3).
    pub actions: Vec<Action>,
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
            actions: cached.actions,
        }
    }
}

/// One selectable line of the actions list (design §2.3, §9.2).
///
/// Flattened rather than nested: every action but `choice` is a single trigger,
/// and `choice` is the one that needs a picker at all — spelling out its options
/// as their own items keeps the whole list one flat, `j`/`k`-navigable thing
/// instead of a second modal layer underneath the first.
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
    /// Flatten a notification's actions into what the list draws and `j`/`k`
    /// walks (design §2.3).
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

    /// The exact reverse of [`Self::next`] — `shift-tab` walks the same ring
    /// the other way, rather than landing somewhere `tab` cannot undo.
    const fn previous(self) -> Self {
        match self {
            Self::Channels => Self::Detail,
            Self::Inbox => Self::Channels,
            Self::Detail => Self::Inbox,
        }
    }
}

/// One entry of the contextual footer/help bar: a key and what it currently
/// does. `label` borrows a `'static` string for the common case and owns one
/// only when the wording depends on runtime state (e.g. an action count).
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
    /// Move focus forward.
    NextPane,
    /// Move focus backward — `shift-tab`, the exact reverse of `tab`.
    PrevPane,
    /// Confirm the highlighted row of the Channels pane (index `0` is
    /// "Tous") as the inbox's actual filter, and jump to the Inbox.
    ///
    /// Separate from moving the cursor there on purpose — see
    /// [`App::applied_channel`] for the bug this exists to fix.
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
    /// Open the guided menu for channel/dnd/device management (§9.2) — a
    /// discoverable on-ramp to commands nobody stumbles onto by trying keys.
    OpenMenu,
    /// Go back one level in the menu, or close it from the root.
    MenuBack,
    /// Act on the menu's highlighted item.
    MenuSelect,
    /// Open the guided compose/publish form (§9.2) — the discoverable
    /// alternative to typing `:publish -c … -t … --action …` from memory.
    OpenCompose,
    /// Step a cyclable field (channel, état, statut, priorité).
    ComposeCycle(i32),
    /// Act on the focused field — see [`ComposeActivation`].
    ComposeActivate,
    /// A typed character, while editing a text field of the form.
    ComposeChar(char),
    /// Backspace while editing a text field of the form.
    ComposeBackspace,
    /// Submit the `:` line as a `reply`, not as a command — see
    /// [`App::reply_for`].
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
    /// Cursor in the Channels pane. `0` is the synthetic "Tous" row drawn
    /// ahead of `channels`; `i` (`i > 0`) is `channels[i - 1]`. Moving this
    /// does **not** change what the Inbox shows — see [`Self::applied_channel`].
    pub selected_channel: usize,
    /// The filter actually applied to the Inbox, indexed exactly like
    /// [`Self::selected_channel`] but changed only by [`Message::SelectChannel`],
    /// never by moving the cursor.
    ///
    /// Splitting the two fixes a real bug: before this field existed, whether
    /// the inbox was filtered at all depended on an invisible combination of
    /// `focus` and cursor position (`focus == Pane::Channels ||
    /// selected_channel > 0`), with nothing on screen saying which channel —
    /// if any — was actually active. Now "no filter" is always index `0` and
    /// only [`Self::channel_filter`] and [`Self::filter_label`] read it.
    pub applied_channel: usize,
    pub selected_row: usize,
    /// How far the Détail pane's body is scrolled, in lines.
    ///
    /// A `Pane` field and not a per-row one: it is where the *view* is, not a fact
    /// about the notification, and it resets the moment the selection moves to a
    /// different one (§9.2) — staying scrolled into notification A's body while
    /// looking at B would read as a rendering bug, not a feature.
    pub detail_scroll: u16,
    /// Which action is highlighted, when the Détail pane's action list is open
    /// (design §2.3, §9.2). `None` is "not open" as well as "no actions to open" —
    /// [`Self::action_items`] answers which, so nothing else has to.
    pub selected_action: Option<usize>,
    /// The guided channel/dnd/device menu (§9.2), when it is open. `None` is
    /// "not open", same idiom as `command`/`show_help`/`selected_action` — at
    /// most one of these is ever `Some` at a time.
    pub menu: Option<menu::Menu>,
    /// The guided compose/publish form (§9.2), when it is open. Same
    /// mutually-exclusive-`Option` idiom as `menu`/`selected_action`.
    pub compose: Option<compose::ComposeForm>,
    /// The notification a `reply` action is answering, while the `:` line below is
    /// being reused as its text box rather than as a shell command.
    ///
    /// A separate field and not a third state on `command` itself: the line editing
    /// (`CommandChar`, `CommandBackspace`, history) is identical either way, only
    /// what `Enter` does at the end differs — see [`Message::SubmitReply`].
    pub reply_for: Option<String>,
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

    /// Turn a key into a message.
    ///
    /// Separate from [`Self::update`] so the bindings can be tested without a
    /// terminal, and so the command line can swallow keys without every binding
    /// having to check whether it is open.
    pub(crate) fn key(&self, code: KeyCode, modifiers: KeyModifiers) -> Message {
        // `q` always quits — a popup, the action list, the menu or the
        // compose form's navigation mode must not swallow it into a mere
        // "close this" the way `esc` does. The one exception is real text
        // entry, where `q` has to type the letter instead: the `:` line, and
        // a compose field mid-edit.
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
                // The line means two different things depending on why it is open
                // — see `App::reply_for`.
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
                // `q` is handled above this tier now — `quit`, not `close`.
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        // The action list, open over the Détail pane (§2.3): `j`/`k` walk it
        // instead of scrolling the body, and only `Enter`/`Esc` mean anything else
        // while it is up — same "swallow everything else" shape as the two blocks
        // above, and for the same reason (`q` quitting mid-pick would cost the
        // pick).
        if self.selected_action.is_some() {
            return match code {
                KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
                KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
                KeyCode::Enter => Message::TriggerAction,
                KeyCode::Esc => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        // The guided menu, same "swallow everything else" shape as the action
        // list above — `Esc` here means "back one level", not "close outright",
        // which is why it is its own message rather than `Cancel`.
        if self.menu.is_some() {
            return match code {
                KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
                KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
                KeyCode::Enter => Message::MenuSelect,
                KeyCode::Esc => Message::MenuBack,
                _ => Message::Ignored,
            };
        }

        // The compose form: two sub-modes, not one. While typing into a text
        // field, every character goes into it — `j`/`k` type the letters `j`
        // and `k`, they do not move focus, which is exactly why this must be
        // its own tier rather than a condition inside the ordinary one below.
        // Outside typing, `j`/`k` move focus and `h`/`l` step a cyclable
        // field's value on a second axis, so the two never compete for a key.
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
                // Discards the whole form, same `Cancel` as every other modal —
                // unlike the menu's `Esc`, there is no "one level up" here.
                KeyCode::Esc => Message::Cancel,
                _ => Message::Ignored,
            };
        }

        match code {
            // `q` is handled above this tier now, unconditionally.
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Message::Quit,
            KeyCode::Char('j') | KeyCode::Down => Message::Move(1),
            KeyCode::Char('k') | KeyCode::Up => Message::Move(-1),
            KeyCode::Char('m') => Message::OpenMenu,
            KeyCode::Char('p') => Message::OpenCompose,
            // `shift-tab` arrives as `BackTab` on most terminals, but some
            // report it as `Tab` with the shift modifier set instead — both
            // are handled so the binding is not terminal-dependent.
            KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => Message::PrevPane,
            KeyCode::Tab => Message::NextPane,
            KeyCode::BackTab => Message::PrevPane,
            // Enter reads as "open this": on the Channels pane that means
            // confirming the highlighted row as the filter (§9.2 — this is what
            // makes the active filter a deliberate choice rather than wherever
            // the cursor happened to be); on the Inbox it still moves focus to
            // Détail. On Détail there is nothing left to open — `Tab` still
            // cycles all the way round if that is what was wanted.
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
                // `back` reports `false` only once there was nothing left to
                // pop but the root — that is the menu closing, not an error.
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
            Message::StoreChanged => self.reload_from_store(session, store).await,
            // Nothing to do for either: a tick exists to let the loop repaint, and
            // an unbound key is deliberately silent rather than beeping.
            Message::Tick | Message::Ignored => {}
        }
    }

    /// Act on the guided menu's highlighted item (design §9.2). Split out of
    /// [`Self::update`] for the same reason [`Self::trigger_action`] is.
    async fn handle_menu_select(&mut self, session: &Session, store: &Store) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        match menu.activate() {
            // The menu mutated itself; nothing else to do until the next
            // `MenuSelect`/`MenuBack` on the new level.
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

    /// Open the compose form, or say why not — the pure half of
    /// [`Message::OpenCompose`].
    fn open_compose(&mut self) {
        if self.channels.is_empty() {
            // A form with a channel picker over nothing to pick is a form
            // nobody can submit — say why instead of opening it.
            "crée ou rejoins un canal avant de publier (m → Canaux)".clone_into(&mut self.status);
        } else {
            self.compose = Some(compose::ComposeForm::new(
                self.channels.len(),
                self.applied_channel.saturating_sub(1),
            ));
        }
    }

    /// Act on the compose form's focused field (design §9.2). Split out of
    /// [`Self::update`] only to keep that match arm-sized — same reason
    /// [`Self::trigger_action`] is its own method.
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

    /// Close whatever popup, list or form is open — [`Message::Cancel`]'s
    /// whole job, split out only to keep `update`'s match arm-sized.
    fn close_modals(&mut self) {
        self.command = None;
        self.reply_for = None;
        self.output = None;
        self.show_help = false;
        self.selected_action = None;
        self.menu = None;
        self.compose = None;
    }

    /// Confirm the highlighted Channels row as the applied filter, and hand
    /// focus to the Inbox — the pure half of [`Message::SelectChannel`],
    /// split out so it is testable without a `Session`/`Store`.
    fn select_channel(&mut self) {
        self.applied_channel = self.selected_channel;
        self.focus = Pane::Inbox;
    }

    fn move_selection(&mut self, delta: i32) {
        // The compose form, when it is open, owns `j`/`k` outright — same
        // precedence `key()` itself gives it over everything below. `key()`
        // only ever emits `Move` here while the form is *not* in edit mode,
        // so this is never called with a character mid-typing.
        if let Some(form) = &mut self.compose {
            form.move_focus(delta);
            return;
        }

        // The menu, when it is open, owns `j`/`k` outright — same precedence
        // `key()` itself gives it over everything below.
        if let Some(menu) = &mut self.menu {
            menu.move_cursor(delta);
            return;
        }

        // The action list, when it is open, is the innermost thing `j`/`k` can be
        // about — it takes priority over both the inbox selection and the detail
        // scroll below it.
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

        // Détail focused: scroll its body instead of moving the inbox selection
        // underneath it. This is the bug §9.2 exists to not have — `j`/`k` used to
        // always move `selected_row`, so reading a long notification silently
        // jumped to the next one the moment the body outgrew the pane. The upper
        // bound is clamped against the actual line count when it is drawn
        // (`render::detail`), not here — this method has no reach into layout.
        if self.focus == Pane::Detail {
            // A page of scroll is bounded well under `u16::MAX` lines by any
            // terminal anybody runs this in, so a delta that somehow didn't fit is
            // clamped rather than reasoned about further.
            let magnitude = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
            self.detail_scroll = if delta.is_negative() {
                self.detail_scroll.saturating_sub(magnitude)
            } else {
                self.detail_scroll.saturating_add(magnitude)
            };
            return;
        }

        let (index, len) = match self.focus {
            // `+ 1` for the synthetic "Tous" row at index 0 — moving the cursor
            // here never touches `applied_channel` (see its doc comment).
            Pane::Channels => (&mut self.selected_channel, self.channels.len() + 1),
            Pane::Inbox => (&mut self.selected_row, self.inbox.len()),
            Pane::Detail => unreachable!("handled above"),
        };

        if len == 0 {
            return;
        }

        let previous = *index;
        // Saturating on both ends rather than casting through a signed type: a
        // selection that wrapped to `usize::MAX` would index nothing and paint an
        // empty detail pane with no explanation.
        *index = if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (*index + delta.unsigned_abs() as usize).min(len - 1)
        };

        // A different notification is now selected: last notification's scroll
        // position has nothing to do with this one's.
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

    /// Do whatever the highlighted action means (design §2.3).
    ///
    /// `view` never reaches the server — it is client-side only, same as the web
    /// client's own `<a href>` (§10.3) — and `reply` reaches it only once the text
    /// typed into the reused `:` line is submitted (`Message::SubmitReply`), not
    /// here. Everything else is one call, after which the list closes: an action
    /// just taken is not a thing to take twice by accident.
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
                // Not a network call: the command line becomes the text box, and
                // `Message::SubmitReply` is where this actually goes out.
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
                // Never selected directly — `ActionItem::list` expands a `choice`
                // into its options, and only those reach here as `ChoiceOption`.
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

        // Both halves are a `String` a person reads in the status line — the
        // difference between them was already spent choosing the wording above.
        self.status = outcome.unwrap_or_else(|message| message);
        self.refresh(session, store).await;
    }

    /// Ask the server how much is unread, per channel — the sidebar badge.
    ///
    /// Split out of [`Self::refresh`] because [`Self::reload_from_store`] wants it
    /// too: unread counts are server-authoritative (design §7.3 — reading in two
    /// inboxes and marking read in one has to be reflected in the other), so
    /// nothing short of asking answers correctly, unlike the inbox rows themselves,
    /// which the local cache already holds.
    async fn refresh_unread(&mut self, session: &Session) {
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
    }

    /// Redraw from the local cache — no request for the inbox rows themselves
    /// (design §9.3bis). What a front end that lost the election does, and what a
    /// poke from the holder triggers.
    ///
    /// The channel *list* is left alone: it comes from the server and does not
    /// change because a notification landed. Unread *counts* are a different
    /// matter — they are exactly what changes when one lands — so this does make
    /// one request for those, mirroring the web client's own `unread.refresh()` on
    /// every stream event: an arrival "only says *ask again*", and a burst of them
    /// collapses into the one `StoreChanged` this is called from, not one request
    /// per notification.
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
        // `channels.len()`, not `len() - 1`: index `channels.len()` is still a
        // valid row, the last real channel, once the synthetic "Tous" row at 0
        // is accounted for.
        self.selected_channel = self.selected_channel.min(self.channels.len());
        self.applied_channel = self.applied_channel.min(self.channels.len());
    }

    /// Which channel the inbox is narrowed to, if any — [`Self::applied_channel`]
    /// alone, never the cursor or which pane has focus.
    fn channel_filter(&self) -> Option<String> {
        self.applied_channel
            .checked_sub(1)
            .and_then(|index| self.channels.get(index))
            .map(|channel| channel.id.as_str().to_owned())
    }

    /// What the Inbox/Détail titles show, so the active filter stays visible
    /// even when the Channels pane isn't focused or has scrolled elsewhere.
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

    /// The current notification's actions, flattened for the list (design §2.3).
    ///
    /// Empty either because there is no current notification or because it has
    /// none — [`Message::EnterActions`] does not need to tell the two apart, and
    /// neither does the render side.
    pub(crate) fn action_items(&self) -> Vec<ActionItem> {
        self.current()
            .map(|row| ActionItem::list(&row.actions))
            .unwrap_or_default()
    }

    /// The bindings that mean something right now (lazygit-style: contextual,
    /// not one static global list) — what the footer's hint row shows.
    ///
    /// Walks the *same* priority tiers as [`Self::key`], in the same order,
    /// so the two cannot silently disagree about which mode is active: a
    /// hint that appears here always corresponds to a key `key()` actually
    /// handles in that state.
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
            // Not `esc/enter/q` any more: `q` quits the whole TUI now, from
            // here as from everywhere else that isn't actual text entry.
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
                // `q` types here — nothing to advertise beyond the field itself.
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

    /// The full keybinding reference for the `?` popup, grouped by where each
    /// binding applies — the same "several small groups" shape as a lazygit
    /// cheat-sheet, and built from the same [`Hint`] wording [`Self::footer_hints`]
    /// uses rather than a second, separately-maintained list.
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

    use notify_client_core::types::ChoiceOption;

    use super::*;

    fn app() -> App {
        App::new("default".to_owned(), true)
    }

    /// A minimal but complete `Channel` — built from JSON rather than the
    /// struct literal because most of its fields (`ChannelPolicy`, `Origin`,
    /// `TtlPolicy`, …) are not re-exported by `notify_client_core::types`, only
    /// `Channel` and `ChannelId` themselves are.
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
        // The bug this pins: `j`/`k` on a long Détail pane used to always move
        // `selected_row`, silently jumping to the next notification.
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
            actions: Vec::new(),
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

    #[test]
    fn the_applied_filter_does_not_follow_focus_or_the_cursor() {
        // The bug this pins: the old `channel_filter` returned a filter whenever
        // `focus == Pane::Channels || selected_channel > 0` — an invisible
        // combination of focus and cursor position, with nothing on screen
        // saying which channel (if any) was actually active. Looking at Détail
        // with the cursor sitting on a channel must not filter anything until
        // that channel has actually been confirmed with `SelectChannel`.
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
        // Drift guard: a hint that appears in the footer but that `key()` ignores
        // is the exact failure mode this refactor exists to prevent.
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
        // The bug this would otherwise be: `j`/`k`/`q` are ordinary bindings,
        // and typing "je viens de finir le job" into the title must not move
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
        // One key, one meaning: `q` quits, full stop — a popup, the action
        // list, the guided menu or the compose form's navigation mode must
        // not swallow it into merely closing themselves the way `esc` does.
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
