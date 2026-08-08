//! The desktop shell (design §10bis.3, milestone M5) — and, structurally as of
//! this file, the shared crate a future Android/iOS build calls into (§10bis.4,
//! milestone M6).
//!
//! # What is deliberately *not* here
//!
//! The tray's state machine and the deep-link parser. They are in
//! `notify_client_core` (`tray`, `deeplink`) because §10bis.3ter ends on the rule
//! that the tray, the dock badge and the web sidebar read "le **même état** de
//! `notify-client-core` — trois rendus, une seule vérité", and because that half can
//! be tested on a machine that cannot build this one at all (§12.4, D13).
//!
//! What is left here is binding: turning a `Tray` into an icon and a menu, a
//! `DeepLink` into a navigation, and keeping that state live by polling
//! `Session::unread`/`Session::dnd` and, when this process wins the stream
//! election (§9.3bis), feeding the shared store the same way the TUI does
//! (`crates/notify-cli/src/tui/mod.rs::run`).
//!
//! # "Live" means polled, not pushed
//!
//! `notify-client-core` has no SSE/stream consumer yet — only the pull-based
//! `GET /sync` (`Session::sync_into`). This shell therefore polls on [`POLL`], the
//! same call `notify ls`/the TUI's `r` key already make, automated. That is not the
//! "temps réel" §10bis.3 asks for, and is worth reading as the more consequential
//! gap the rest of the desktop story now sits on top of (see the README).
//!
//! # `lib.rs` and `main.rs`, and why there are two
//!
//! Everything used to live in `main.rs`. It moved here because Tauri v2 mobile
//! targets link a `staticlib`/`cdylib` produced from a `#[cfg_attr(mobile,
//! tauri::mobile_entry_point)]`-annotated `run()` — a `fn main` cannot be that
//! entry point, Android and iOS do not call one. `main.rs` is now the few lines
//! Tauri's desktop target still needs a binary for; this file is what both targets
//! actually run. That split is the **whole** of this increment's Android work: no
//! platform code was added, nothing here has been built for Android, and nothing
//! claims otherwise — `cargo tauri android init` still needs an SDK/NDK this
//! workspace does not have (§14.4, D33; `docs/upstream/che-min-mise-android.md`).
//! What changed is that the crate is now the shape that command expects to find
//! the day it can run, rather than a desktop-only layout somebody would have had
//! to restructure under a red build later.
//!
//! # First build
//!
//! Linux Tauri links against a GTK/WebKit stack this workspace did not carry until
//! the `che-mise-webkit` dev image landed (§12.4, D13) — `web/src-tauri` is still
//! outside the cargo workspace (see `Cargo.toml`), so no gate in this repository
//! builds it, but `task tauri:build`/`:dev`/`:check` do on a machine `task
//! tauri:doctor` clears. The mobile targets this file is now shaped for have no
//! equivalent doctor yet, because there is nothing in this repository for one to
//! check against until D33's image lands.

use std::time::Duration;

use notify_client_core::types::DndDraft;
use notify_client_core::{
    ClientError, DEFAULT_PROFILE, DeepLink, DndPreset, Link, Profile, Session, Store,
    StreamLease, StreamSignal, Tray, TrayIcon as TrayIconState, now_utc, parse_deep_link,
    quiet_now, try_hold_stream, tray,
};
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuEvent, MenuItem, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter as _, Manager as _, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt as _;

/// How often the tray re-reads the server. See the module doc: this stands in for
/// real push until `notify-client-core` grows a stream consumer, and is a plain
/// judgment call rather than derived from any documented rate limit (§8.2 bounds
/// SSE connections, not `/unread` or `/preferences/dnd`).
const POLL: Duration = Duration::from_secs(20);

/// Build and run the app. Called from `main()` on desktop; called by the mobile
/// runtime on Android/iOS once a build for either exists (see the module doc — it
/// does not yet, D33/M6).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Relaunching focuses the window that already exists (§10bis.3): a second
        // process would hold a second SSE stream against §8.2's ceiling and pop
        // every notification twice.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            focus_main(app);
            // On Windows and Linux a deep link reaches the *second* launch as an
            // argument, which is why it is read here as well as in the handler
            // below — the same click arrives by two different roads per platform.
            if let Some(link) = argv.iter().skip(1).find_map(|arg| parse_deep_link(arg)) {
                navigate(app, &link);
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Every URL the OS hands over goes through the same parser, and
            // anything it does not recognise is dropped rather than routed: this
            // is an input somebody else authored (see `deeplink`).
            let deep_link_handle = handle.clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    if let Some(link) = parse_deep_link(url.as_str()) {
                        navigate(&deep_link_handle, &link);
                    }
                }
            });

            // Shown only after the first paint, with the background token as the
            // window colour, so there is no white flash (§10bis.3bis).
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("weebo-notify")
                .visible(false)
                .build()?;

            // Close hides rather than quits (§10bis.3ter): the app's normal mode of
            // life is the tray. The Linux "no tray host detected, so close really
            // quits" fallback the design also asks for is not implemented yet — on
            // a machine where the tray build below fails, this makes the window
            // reachable only by killing the process, a named cost rather than a
            // silent one.
            let close_window = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = close_window.hide();
                }
            });

            // The synchronous half of startup, done on the main thread before
            // anything async runs, so the tray's very first paint reflects the
            // real (`Disconnected`) starting state rather than nothing at all —
            // §10bis.3bis's "no white flash" extended to the tray icon. Neither
            // call can fail in a way worth more than a line on stderr: a profile
            // is almost never absent (`Profile::load` falls back to a default),
            // and a session that fails to open just leaves the tray static.
            let profile = Profile::load(DEFAULT_PROFILE);
            let session = match &profile {
                Ok(profile) => Session::open(profile).ok(),
                Err(error) => {
                    eprintln!("weebo-notify: no usable profile: {error}");
                    None
                }
            };

            let icons = TrayIcons::load();
            let initial = tray_now(0, false, false);

            let status_item = MenuItemBuilder::with_id("status", &initial.status)
                .enabled(false)
                .build(&handle)?;

            let mut menu_builder = MenuBuilder::new(&handle)
                .item(&status_item)
                .separator()
                .text("open-inbox", "Ouvrir l'inbox");

            // Present only when there is a session to write through — omitted
            // rather than disabled, the same "no dead affordance" reasoning that
            // keeps an inert "Rechercher les mises à jour" off this menu entirely
            // (no updater exists yet to check against).
            if session.is_some() {
                let dnd_menu = SubmenuBuilder::new(&handle, "Ne pas déranger")
                    .text("dnd-1h", "1h")
                    .text("dnd-until-morning", "jusqu'à demain 8h")
                    .text("dnd-off", "désactiver")
                    .build()?;
                menu_builder = menu_builder.separator().item(&dnd_menu);
            }

            // A manual item rather than `PredefinedMenuItem::quit`: muda documents
            // that predefined item as "Linux: Unsupported" — exactly the platform
            // this shell builds on today — so the native item would be a silent
            // no-op click here rather than the affordance §10bis.3bis asks native
            // menus to be.
            let menu = menu_builder.separator().text("quit", "Quitter").build()?;

            let tray_result = TrayIconBuilder::new()
                .icon(icons.for_state(initial.icon))
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(on_menu_event)
                .build(&handle);

            match (tray_result, session) {
                (Ok(tray_icon), Some(session)) => {
                    // Managed so `on_menu_event` — which only ever sees an
                    // `AppHandle`, never this closure's locals — can reach it for
                    // the DND quick actions.
                    app.manage(session.clone());

                    if let Some(daemon) = open_daemon() {
                        let daemon_session = session.clone();
                        // Its own OS thread with its own current-thread runtime,
                        // not `tauri::async_runtime::spawn` — `Store` wraps a
                        // `rusqlite::Connection`, which is `Send` but not `Sync`
                        // (a `RefCell` inside), so a future that holds `&Store`
                        // across an `.await` (as `Session::sync_into` does) is not
                        // `Send` and cannot go on Tauri's multi-thread runtime.
                        // `notify-cli`'s own `block_on` exists for exactly this
                        // reason (`crates/notify-cli/src/main.rs`) — this mirrors
                        // it rather than fighting the same constraint twice.
                        let _ = std::thread::Builder::new()
                            .name("weebo-notify-daemon".to_owned())
                            .spawn(move || run_daemon(daemon_session, daemon));
                    }

                    tauri::async_runtime::spawn(poll_loop(
                        session,
                        icons,
                        tray_icon,
                        status_item,
                        initial,
                    ));
                }
                // No usable profile/session: nothing to poll, so the tray stays at
                // its initial `Disconnected` frame — the "no popup error loop"
                // rule §10bis.3ter states for this icon, extended to the whole
                // startup path rather than just the network calls.
                (Ok(_), None) => {}
                // No tray host on this machine — real on Linux even after
                // `task tauri:doctor` is clean: the appindicator backend `dlopen`s
                // its library at *runtime*, with no build-time probe. The window
                // still works; this run is then an ordinary windowed app, minus
                // the close-to-hide affordance's way back in (see above).
                (Err(error), _) => {
                    eprintln!("weebo-notify: tray unavailable: {error}");
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the desktop shell failed to start");
}

/// The four committed tray glyphs (`icons/tray-*.png`), decoded once at startup.
///
/// `include_bytes!` fails the *build* if a file goes missing — the same "a check
/// that runs is never out of date" instinct the icons' own README already applies
/// to their rasterisation. A `PNG` that exists but fails to decode is a build-time
/// mistake in an already-committed, already-rasterised asset, not a runtime
/// condition, so [`TrayIcons::load`] expects rather than handles it.
struct TrayIcons {
    neutral: Image<'static>,
    unread: Image<'static>,
    dnd: Image<'static>,
    offline: Image<'static>,
}

impl TrayIcons {
    fn load() -> Self {
        Self {
            neutral: Image::from_bytes(include_bytes!("../icons/tray-neutral.png"))
                .expect("icons/tray-neutral.png is a valid PNG"),
            unread: Image::from_bytes(include_bytes!("../icons/tray-unread.png"))
                .expect("icons/tray-unread.png is a valid PNG"),
            dnd: Image::from_bytes(include_bytes!("../icons/tray-dnd.png"))
                .expect("icons/tray-dnd.png is a valid PNG"),
            offline: Image::from_bytes(include_bytes!("../icons/tray-offline.png"))
                .expect("icons/tray-offline.png is a valid PNG"),
        }
    }

    /// The image for one [`TrayIconState`] variant. Cloning an already-decoded
    /// [`Image`] is cheap — these are small icons and the clone is `Cow`-backed —
    /// so decoding happens exactly once, here, rather than on every poll.
    fn for_state(&self, icon: TrayIconState) -> Image<'static> {
        match icon {
            TrayIconState::Neutral => self.neutral.clone(),
            TrayIconState::Unread => self.unread.clone(),
            TrayIconState::DoNotDisturb => self.dnd.clone(),
            TrayIconState::Disconnected => self.offline.clone(),
        }
    }
}

/// What this process owns only when it wins the stream election (§9.3bis).
///
/// Exclusively owned by [`run_daemon`]'s own thread, never shared: [`Store`] wraps
/// a bare `rusqlite::Connection` with no locking of its own, and every existing
/// caller (the TUI) already assumes single-owner use. The tray's own numbers come
/// from `Session::unread`/`Session::dnd`, not the cache, so nothing else in this
/// increment needs to touch it — the daemon behaviour exists purely to keep the
/// shared cache warm for the CLI/TUI while this is the one thing running.
struct Daemon {
    store: Store,
    signal: StreamSignal,
    /// Held only for its `Drop`: the election lock releases when this drops, so
    /// the next launch — or another front end — can win it back. There is no
    /// `release()` method on `StreamLease` on purpose (see `election.rs`).
    _lease: StreamLease,
}

/// Try to become the front end that holds the stream and feeds the shared store.
///
/// `None` covers three different, equally normal outcomes — the local store could
/// not be opened, somebody else already holds the stream, or the signal socket
/// could not be bound — and in every one of them the tray still works from
/// `Session` alone; it just never becomes the daemon. Each is logged distinctly
/// because they point at different fixes (a permissions problem vs. nothing to fix
/// at all), but none of them is worth failing startup over.
fn open_daemon() -> Option<Daemon> {
    let store = match Store::open(DEFAULT_PROFILE) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("weebo-notify: local store unavailable: {error}");
            return None;
        }
    };

    let lease = match try_hold_stream(store.path()) {
        Ok(Some(lease)) => lease,
        // Somebody else already holds it — an ordinary outcome (§9.3bis), not a
        // failure: this process still polls the API for its own tray, it just
        // never becomes the daemon that keeps the shared cache warm.
        Ok(None) => return None,
        Err(error) => {
            eprintln!("weebo-notify: cannot elect for the stream: {error}");
            return None;
        }
    };

    let signal = match StreamSignal::bind(store.path()) {
        Ok(signal) => signal,
        Err(error) => {
            eprintln!("weebo-notify: cannot signal readers: {error}");
            return None;
        }
    };

    Some(Daemon {
        store,
        signal,
        _lease: lease,
    })
}

/// Keep the tray honest, on a timer — see the module doc for why a timer and not a
/// push. Runs on Tauri's own multi-thread runtime (`tauri::async_runtime::spawn`),
/// which is why this never touches [`Store`] — see [`run_daemon`] for the half
/// that does, and why it runs somewhere else entirely.
async fn poll_loop(
    session: Session,
    icons: TrayIcons,
    tray_icon: TrayIcon,
    status_item: MenuItem<tauri::Wry>,
    initial: Tray,
) {
    let mut interval = tokio::time::interval(POLL);
    let mut last_state = initial;
    let mut last_unread = 0u64;
    let mut last_quiet = false;

    loop {
        // Tokio's default `MissedTickBehavior` fires the first tick immediately,
        // so the tray's very first live read happens right after the static
        // `Disconnected` frame built in `setup`, not up to `POLL` later.
        interval.tick().await;

        let unread = session.unread().await;
        let dnd = session.dnd().await;
        let connected = unread.is_ok() && dnd.is_ok();

        // Stale-but-plausible beats a false zero: on a failed tick, keep the last
        // good reading rather than resetting the badge while merely offline —
        // `tray.rs`'s own doc gives the reason a disconnected client cannot vouch
        // for a fresh number anyway.
        if let Ok(counts) = &unread {
            last_unread = counts
                .iter()
                .map(|count| u64::try_from(count.unread.max(0)).unwrap_or(0))
                .sum();
        }
        if let (Ok(preference), Ok(now)) = (&dnd, now_utc()) {
            last_quiet = quiet_now(preference.until, now);
        }

        let state = tray_now(last_unread, last_quiet, connected);
        if state != last_state {
            let _ = tray_icon.set_icon(Some(icons.for_state(state.icon)));
            let _ = status_item.set_text(&state.status);
            last_state = state;
        }
    }
}

/// Keep the shared store warm for the CLI/TUI (§9.3bis), on its own thread.
///
/// A plain OS thread with a private current-thread `tokio` runtime — the same
/// shape `notify-cli`'s own `block_on` uses (`crates/notify-cli/src/main.rs`), and
/// for the same reason: `Store` wraps a `rusqlite::Connection`, `Send` but not
/// `Sync`, so a future that holds `&Store` across an `.await` (as
/// `Session::sync_into` does) cannot run on a multi-thread runtime. Nothing here
/// touches the tray — that is [`poll_loop`]'s job, running concurrently on Tauri's
/// own runtime.
fn run_daemon(session: Session, mut daemon: Daemon) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("weebo-notify: cannot start the daemon runtime: {error}");
            return;
        }
    };

    runtime.block_on(async move {
        let mut interval = tokio::time::interval(POLL);
        loop {
            interval.tick().await;

            let device = daemon.store.device().unwrap_or(None);
            match session
                .sync_into(&daemon.store, None, device.as_deref())
                .await
            {
                Ok(outcome) if !outcome.is_quiet() => daemon.signal.notify(),
                Ok(_) => {}
                Err(error) => eprintln!("weebo-notify: sync: {error}"),
            }
        }
    });
}

/// Dispatch a tray menu click. Registered once, in `setup`, on the tray itself —
/// this only ever sees clicks from that one menu in this increment.
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "open-inbox" => focus_main(app),
        "quit" => app.exit(0),
        id @ ("dnd-1h" | "dnd-until-morning" | "dnd-off") => {
            let Some(session) = app.try_state::<Session>() else {
                return;
            };
            let session = session.inner().clone();
            let preset = match id {
                "dnd-1h" => DndPreset::OneHour,
                "dnd-until-morning" => DndPreset::UntilMorning,
                _ => DndPreset::Off,
            };
            tauri::async_runtime::spawn(async move {
                if let Err(error) = write_dnd(&session, preset).await {
                    eprintln!("weebo-notify: dnd: {error}");
                }
            });
        }
        _ => {}
    }
}

/// Round-trip a quick DND choice through the same read-before-write
/// `notify-cli`'s `notify dnd` uses (`crates/notify-cli/src/manage.rs::dnd`): the
/// endpoint replaces the preference whole, so writing only `until` would silently
/// drop somebody's `quiet_hours` schedule.
///
/// Deliberately does not push an immediate tray update — that would need a shared
/// last-applied-state cell between this and [`poll_loop`] for a cosmetic win. Every
/// other client (the web sidebar, the TUI) already learns of the change instantly
/// via the `preference` SSE event (§8.1); this shell's own icon catches up on its
/// next poll.
async fn write_dnd(session: &Session, preset: DndPreset) -> Result<(), ClientError> {
    let current = session.dnd().await?;
    // The preference's own zone, not this machine's (§7.4): somebody travelling
    // keeps their own mornings. A preference with no schedule has no zone to
    // borrow, and UTC is the honest answer rather than a guess from the laptop —
    // the same fallback `notify-cli`'s `dnd on --until-morning` uses.
    let timezone = current
        .quiet_hours
        .first()
        .map_or("UTC", |window| window.timezone.as_str());
    let until = preset.until(now_utc()?, timezone)?;

    session
        .set_dnd(&DndDraft {
            until,
            quiet_hours: current.quiet_hours,
            scope: current.scope,
            exceptions: current.exceptions,
            urgent_bypasses: current.urgent_bypasses,
        })
        .await?;

    Ok(())
}

/// Bring the existing window forward.
fn focus_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        // Un-minimise before focusing: a focused-but-minimised window is the
        // "nothing happened" bug of every single-instance implementation.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// The event the front end listens for. Shared with `web/src/lib/deeplink.ts`.
const DEEP_LINK_EVENT: &str = "deep-link";

/// Ask the front end to go where a link points.
///
/// # Why this emits instead of driving the router
///
/// The first draft `eval`-ed a `pushState` into the webview and fired a synthetic
/// `popstate` after it. That works, and it reaches *past* the Vue router into the
/// History API — so every guard the router runs is skipped. `/n/<id>` opened while
/// signed out is supposed to go through the login guard and come back afterwards
/// (`web/src/router/index.ts`), and the `eval` version would have dropped somebody
/// on a blank detail panel instead.
///
/// The path comes from [`DeepLink::path`] rather than being rebuilt here: the router
/// already owns `/n/<id>` and `/c/<id>` (§10.4), and a second spelling in the shell
/// is how the desktop and the web drift apart.
fn navigate(app: &tauri::AppHandle, link: &DeepLink) {
    focus_main(app);
    let _ = app.emit(DEEP_LINK_EVENT, serde_json::json!({ "path": link.path() }));
}

/// What the tray shows, from the one state everything else reads.
fn tray_now(unread: u64, quiet: bool, connected: bool) -> Tray {
    tray(
        unread,
        quiet,
        if connected {
            Link::Connected
        } else {
            Link::Disconnected
        },
    )
}
