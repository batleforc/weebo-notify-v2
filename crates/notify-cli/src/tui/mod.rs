//! The terminal UI (§9.2), Elm-style: only `update` changes state, `draw` has no side effects,
//! and network work runs in tasks that send `Message`s. The `:` line feeds the CLI's own clap
//! parser. The panic hook restores the terminal, and server strings are sanitised before drawing.

mod app;
mod completion;
mod compose;
mod desktop;
mod history;
mod menu;
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

/// How long the loop waits for a key before ticking (the tick advances the clock and polls the store).
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

    // The election (§9.3bis): the holder feeds the store from the stream; losers read it.
    let lease = notify_client_core::try_hold_stream(store.path())?;

    // The holder pokes the socket, others reload when poked; the tick covers a missing signal.
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

/// This window's half of the election (§9.3bis).
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

/// Put the terminal into full-screen mode, and arrange to undo it whatever happens.
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

/// Undo it. Safe to call twice (it runs from the panic hook).
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

    // First paint from the server: a blank inbox that fills later reads as broken.
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
            // The tick also covers a missing socket: the loop still repaints and `r` still works.
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

/// Hand arrivals to the desktop when `--notify-send` asked for it (§9.2); I/O, so outside `update`.
fn relay_arrivals(front: &Front<'_>, app: &mut App) {
    // Drained regardless of the flag, or they'd accumulate unread.
    let arrivals = app.take_arrivals();
    if !front.notify_send {
        return;
    }

    for row in arrivals {
        // The server computed this per reader (§7.4); don't re-derive it.
        if row.muted_by_dnd {
            continue;
        }
        desktop::relay(&row.title, row.body.as_deref().unwrap_or_default());
    }
}

/// Tell the readers if this front end just wrote and holds the signal (I/O, so outside `update`).
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

/// Strip control sequences from anything the server sent (§2.3, §9.2): the server strips too,
/// but the terminal is the renderer here.
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
        // A space, not nothing: no silently joined words, and the tampering stays visible.
        assert_eq!(sanitise("a\u{7}b"), "a b");
    }
}
