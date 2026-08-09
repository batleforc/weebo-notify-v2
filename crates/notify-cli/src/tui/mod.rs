//! The terminal UI (design §9.2).
//!
//! # Elm-like, because the alternative is a redraw in an I/O handler
//!
//! One immutable-ish [`App`], one [`Message`] enum, an `update` that is the only
//! thing allowed to change state, and a `draw` with no side effects. Network work
//! lives in tokio tasks that push `Message`s down a channel; the render loop never
//! awaits a socket. That is the pattern ratatui recommends, and the reason is the
//! failure it prevents: a redraw called from inside a response handler paints half
//! a frame from one state and half from another.
//!
//! # The `:` mode is the parity mechanism, not a feature
//!
//! §9 promises that everything the CLI can do, the TUI can do. That promise is not
//! maintained by hand: `:` opens a line that is split with shell rules and handed
//! to **the same clap parser** the shell uses. A subcommand added to the CLI is
//! therefore available here by construction — nobody has to remember to wire it.
//!
//! # One front end writes, the others are told
//!
//! Whoever wins the election of §9.3bis feeds the shared store from the stream and
//! pokes a socket beside it after each write; the others render **from the store**
//! when poked, without a request of their own. That is what keeps three windows on
//! one laptop inside the twenty stream slots a person gets (§8.2), and the footer
//! says which of the two roles this window has.
//!
//! # Two rules that are not decoration
//!
//! * **the panic hook restores the terminal first.** A bug that leaves somebody's
//!   shell in raw mode with no echo is a bug they reboot a terminal over;
//! * **every string from the server is stripped of control sequences before it is
//!   drawn** (§2.3). The server strips at ingestion; this is the second gate, and
//!   it is the one that matters when the terminal is the renderer.

mod app;
mod completion;
mod desktop;
mod history;
mod render;

use std::io::IsTerminal as _;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify_client_core::{Profile, Session, Store, StreamSignal, StreamWatch};

use crate::CliError;
pub(crate) use app::{ActionItem, App, Message, Pane};

/// How long the loop waits for a key before ticking.
///
/// A tick is what advances the clock and polls the store; without one the UI would
/// only ever repaint when somebody pressed something.
const TICK: Duration = Duration::from_millis(250);

/// Open the interface.
pub(crate) fn run(profile_name: &str, notify_send: bool) -> Result<(), CliError> {
    if !std::io::stdout().is_terminal() {
        return Err(CliError::Usage(
            "stdout is not a terminal — refusing to open the TUI".to_owned(),
        ));
    }

    let profile = Profile::load(profile_name)?;
    let session = Session::open(&profile)?;
    let store = Store::open(profile_name)?;

    // The election (§9.3bis): whoever holds this feeds the store from the stream.
    // Losing is normal — another front end is already doing it, and this one reads
    // what that one writes.
    let lease = notify_client_core::try_hold_stream(store.path())?;

    // Two roles, two ends of the same socket. The holder binds it and pokes; anybody
    // else watches it and reloads from the store when poked. Neither is load-bearing:
    // without the signal the tick below still gets there, a moment later.
    let (signal, watch) = if lease.is_some() {
        (Some(StreamSignal::bind(store.path())?), None)
    } else {
        (None, Some(StreamWatch::watch(store.path())))
    };

    let mut terminal = enter()?;
    let outcome = event_loop(
        &mut terminal,
        &session,
        &store,
        Front {
            profile: profile_name,
            holds_stream: lease.is_some(),
            signal,
            watch,
            notify_send,
        },
    );
    leave();

    outcome
}

/// This window's half of the election (§9.3bis), kept together so the loop below
/// does not take six arguments.
struct Front<'a> {
    profile: &'a str,
    holds_stream: bool,
    /// Set when this front end holds the stream: it pokes the others.
    signal: Option<StreamSignal>,
    /// Set when it does not: it waits to be poked.
    watch: Option<StreamWatch>,
    /// Whether arrivals also go to the desktop (§9.2, `--notify-send`).
    notify_send: bool,
}

/// Put the terminal into the state a full-screen app needs, and arrange to undo it
/// whatever happens.
fn enter() -> Result<ratatui::DefaultTerminal, CliError> {
    // Installed before raw mode: a panic between the two must still restore.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        previous(info);
    }));

    enable_raw_mode().map_err(|error| terminal_failure(&error))?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)
        .map_err(|error| terminal_failure(&error))?;

    ratatui::try_init().map_err(|error| terminal_failure(&error))
}

/// Undo it. Safe to call twice, which is what makes it usable from a panic hook.
fn leave() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
    ratatui::restore();
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    session: &Session,
    store: &Store,
    mut front: Front<'_>,
) -> Result<(), CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::Api(format!("could not start the async runtime: {error}")))?;

    let mut app = App::new(front.profile.to_owned(), front.holds_stream);

    // The first paint comes from the server rather than from an empty screen: an
    // inbox that appears blank and fills in a second later reads as broken.
    runtime.block_on(app.refresh(session, store));
    poke(&mut front, &mut app);

    loop {
        terminal
            .draw(|frame| render::draw(frame, &app))
            .map_err(|error| terminal_failure(&error))?;

        if event::poll(TICK).map_err(|error| terminal_failure(&error))? {
            if let Event::Key(key) = event::read().map_err(|error| terminal_failure(&error))?
                && key.kind == KeyEventKind::Press
            {
                let message = app.key(key.code, key.modifiers);
                runtime.block_on(app.update(message, session, store));
            }
        } else {
            // A reader's tick is where the signal is read. It is also the net under
            // it: if the socket is gone — no holder yet, or a platform without unix
            // sockets — the loop still repaints and `r` still works.
            let message = if front.watch.as_mut().is_some_and(StreamWatch::poked) {
                Message::StoreChanged
            } else {
                Message::Tick
            };
            runtime.block_on(app.update(message, session, store));
        }

        poke(&mut front, &mut app);
        relay_arrivals(&front, &mut app);

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Hand what arrived to the desktop, when `--notify-send` asked for it (§9.2).
///
/// Outside `App::update` for the reason `poke` is: `update` changes state and stays
/// free of I/O, and a popup is I/O. The do-not-disturb gate is §7.4's, and it is
/// applied here rather than server-side because the stream keeps flowing during a
/// quiet period — an open client is a deliberate choice to look.
fn relay_arrivals(front: &Front<'_>, app: &mut App) {
    // Drained before the flag is checked, and that order is deliberate: without it
    // a TUI running without `--notify-send` would accumulate every arrival of the
    // session in a vector nobody reads.
    let arrivals = app.take_arrivals();
    if !front.notify_send {
        return;
    }

    for row in arrivals {
        // The server already answered this, per reader, on the event that carried
        // the arrival (§7.4). Asking again here is what D22 was about: the rule
        // lives with the preference, the server applies it once, and a client that
        // re-derived it was a third copy to keep in step.
        if row.muted_by_dnd {
            continue;
        }
        desktop::relay(&row.title, row.body.as_deref().unwrap_or_default());
    }
}

/// Tell the readers, if this front end just wrote and holds the signal.
///
/// Outside `App::update` on purpose: `update` is the only thing allowed to change
/// state and it stays free of I/O, so the effect happens here where the loop can see
/// it (design §9.2).
fn poke(front: &mut Front<'_>, app: &mut App) {
    if app.take_wrote()
        && let Some(signal) = front.signal.as_mut()
    {
        signal.notify();
    }
}

fn terminal_failure(error: &std::io::Error) -> CliError {
    CliError::Api(format!("the terminal refused: {error}"))
}

/// Strip control sequences from anything the server sent (§2.3, §9.2).
///
/// Defence in depth: the server already strips at ingestion, and this is the gate
/// that matters here — a title carrying `\u{1b}[2J` would clear the screen of
/// whoever is reading their inbox.
pub(crate) fn sanitise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            // An escape swallows the sequence that follows it, up to its final byte.
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            other if other.is_control() => out.push(' '),
            other => out.push(other),
        }
    }

    out
}

/// Keys the help bar advertises, in the order it shows them.
pub(crate) const BINDINGS: &[(&str, &str)] = &[
    ("j/k", "naviguer / défiler"),
    ("tab", "panneau"),
    ("enter", "détail"),
    ("a", "actions"),
    ("r", "rafraîchir"),
    (":", "commande"),
    ("?", "aide"),
    ("q", "quitter"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_cannot_clear_the_screen_of_whoever_reads_it() {
        assert_eq!(sanitise("etcd \u{1b}[2Jdegraded"), "etcd degraded");
        assert_eq!(sanitise("colour \u{1b}[31mred"), "colour red");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        assert_eq!(
            sanitise("Pipeline #4521 — succès"),
            "Pipeline #4521 — succès"
        );
    }

    #[test]
    fn a_stray_control_character_becomes_a_space_rather_than_vanishing() {
        // Vanishing would silently join two words; a space keeps the text readable
        // and makes the tampering visible.
        assert_eq!(sanitise("a\u{7}b"), "a b");
    }
}
