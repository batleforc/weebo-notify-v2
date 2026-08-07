//! The desktop shell (design §10bis.3, milestone M5).
//!
//! # What is deliberately *not* here
//!
//! The tray's state machine and the deep-link parser. They are in
//! `notify_client_core` (`tray`, `deeplink`) because §10bis.3ter ends on the rule
//! that the tray, the dock badge and the web sidebar read "le **même état** de
//! `notify-client-core` — trois rendus, une seule vérité", and because that half can
//! be tested on a machine that cannot build this one at all (§12.4, D13).
//!
//! What is left here is binding: turning a `Tray` into an icon and a menu, and a
//! `DeepLink` into a navigation. That is the part a webview is genuinely needed for.
//!
//! # This file has never been compiled
//!
//! Said once, here, rather than discovered: Linux Tauri links against a GTK/WebKit
//! stack that this workspace does not carry, so `web/src-tauri` is outside the cargo
//! workspace and no gate in this repository builds it (see `Cargo.toml` next door,
//! and `task tauri:doctor`). Treat every line below as a proposal until the first
//! build says otherwise.

// Windows: no console window behind the app in a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use notify_client_core::{DeepLink, Link, Tray, TrayIcon, parse_deep_link, tray};
use tauri::{Emitter as _, Manager as _, WebviewUrl, WebviewWindowBuilder};

fn main() {
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
            app.deep_link().on_open_url(move |event| {
                // Every URL the OS hands over goes through the same parser, and
                // anything it does not recognise is dropped rather than routed:
                // this is an input somebody else authored (see `deeplink`).
                for url in event.urls() {
                    if let Some(link) = parse_deep_link(url.as_str()) {
                        navigate(&handle, &link);
                    }
                }
            });

            // Shown only after the first paint, with the background token as the
            // window colour, so there is no white flash (§10bis.3bis).
            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("weebo-notify")
                .visible(false)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the desktop shell failed to start");
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

/// Render the shared tray state into this platform's icon and menu.
///
/// Kept as a free function taking a [`Tray`] so that what it renders is decided by
/// `notify_client_core::tray` and tested there — this only maps a variant onto an
/// asset.
#[allow(dead_code, reason = "bound once the tray is built; see the module note")]
fn tray_asset(state: &Tray) -> &'static str {
    match state.icon {
        TrayIcon::Neutral => "icons/tray-neutral.png",
        TrayIcon::Unread => "icons/tray-unread.png",
        TrayIcon::DoNotDisturb => "icons/tray-dnd.png",
        TrayIcon::Disconnected => "icons/tray-offline.png",
    }
}

/// What the tray shows, from the one state everything else reads.
#[allow(dead_code, reason = "bound once the tray is built; see the module note")]
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
