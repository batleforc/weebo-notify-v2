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
//! actually run.
//!
//! # What mobile does not do yet
//!
//! Android now builds — `task android:build` produces an APK and an AAB — but what
//! it builds is deliberately small, and the `#[cfg(desktop)]` density below is the
//! honest map of it. On a phone this shell is a webview and a deep-link handler.
//! Everything else is desktop-gated, in two groups and for two different reasons:
//!
//! - **Cannot exist there.** `tauri::menu` and `tauri::tray` are `#[cfg(desktop)]`
//!   in Tauri itself, and `tauri-plugin-single-instance` has no `init` on mobile.
//!   The tray, its menu and the DND quick actions hanging off it therefore have no
//!   mobile form at all; Android's equivalent is a notification action, which is a
//!   feature nobody has designed yet rather than a port of this one.
//! - **Would be wrong there.** The store-warming daemon (§9.3bis) exists to keep the
//!   shared SQLite cache warm *for the CLI and the TUI* — neither of which exists on
//!   a phone — and a background thread polling every 20s is not how Android wants
//!   work done anyway. It is gated off rather than left to fail quietly.
//!
//! So: no tray, no menu, no DND shortcut, no polling and no daemon on mobile. The
//! notification plugin is registered and its `POST_NOTIFICATIONS` permission is
//! merged in from the plugin's own manifest, but nothing in this file posts one yet
//! — the shell has no push consumer on any platform (see "Live means polled").
//!
//! # First build
//!
//! Linux Tauri links against a GTK/WebKit stack this workspace did not carry until
//! the `che-mise-webkit` dev image landed (§12.4, D13) — `web/src-tauri` is still
//! outside the cargo workspace (see `Cargo.toml`), so no gate in this repository
//! builds it, but `task tauri:build`/`:dev`/`:check` do on a machine `task
//! tauri:doctor` clears, and `task android:build` does on one `task android:doctor`
//! clears. Neither is run by CI: D13's reasoning is unchanged, and an Android build
//! additionally wants ~3 GB of SDK that no gate should be downloading.

// Split by platform, and the size of the two halves is the honest summary of where
// the mobile shell is: `tauri::menu` and `tauri::tray` are `#[cfg(desktop)]` in
// Tauri itself, so the tray, its menu, the DND quick actions and the store-warming
// daemon are all desktop-only surface. What Android and iOS share with them is a
// webview and a deep link, and that is the whole list — see the module doc's
// "What mobile does not do yet".
use notify_client_core::{DeepLink, parse_deep_link};
use tauri::window::Color;
use tauri::{Emitter as _, Manager as _, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt as _;

// Desktop-only, and not an omission on mobile: the loopback listener of RFC 8252
// is the answer to "a packaged shell has no registrable origin", and a phone
// answers that with its own registered scheme through the deep link above —
// Android and iOS both hand `weebo-notify://oauth/callback` back to the app the
// system browser was launched from, so there is nothing to listen on.
// Desktop-only for the same reason `oauth` is, and one more: the vault this wraps
// is the desktop trio (Secret Service / Keychain / Credential Manager). Mobile has
// its own — Keystore and the iOS Keychain (§10bis.2) — and reaching them is not the
// same call, so a shared module here would be a lie about what is implemented.
#[cfg(desktop)]
mod credential;

#[cfg(desktop)]
mod oauth;

#[cfg(desktop)]
use std::sync::{Arc, Mutex};
#[cfg(desktop)]
use std::time::Duration;

#[cfg(desktop)]
use notify_client_core::types::DndDraft;
#[cfg(desktop)]
use notify_client_core::{
    ClientError, DEFAULT_PROFILE, DndPreset, Link, Profile, Session, Store, StreamLease,
    StreamSignal, Tray, TrayIcon as TrayIconState, now_utc, quiet_now, tray, try_hold_stream,
};
#[cfg(desktop)]
use tauri::AppHandle;
#[cfg(desktop)]
use tauri::image::Image;
#[cfg(desktop)]
use tauri::menu::{MenuBuilder, MenuEvent, MenuItem, MenuItemBuilder, SubmenuBuilder};
#[cfg(desktop)]
use tauri::tray::{TrayIcon, TrayIconBuilder};

/// The credential the Rust half of the shell runs on, swappable while it runs.
///
/// It used to be a `Session` built once in `setup` from `Profile::load` and moved
/// into the tray loop, which made the tray's authentication a fact about *startup*.
/// That is why signing in through the window left the tray saying
/// `auth.invalid_credential` forever: `Profile.token` is written by `notify login`
/// and by nothing else, so the webview's own session — the one the person just
/// completed — was invisible to this half of the process.
///
/// # Why not mint an API key instead
///
/// The obvious repair is for the shell to mint a personal API key after signing in,
/// the way `notify login` has a person do by hand (§9.1). It does not work here, and
/// the reason is in the domain rather than in the plumbing: `NewApiKey.channels`
/// cannot be empty (`notify_app::keys`), so every key names a fixed list of channels
/// — while the tray counts unread across *all* of them. A key minted at sign-in
/// would under-report the day its owner joins a channel, and silently.
///
/// `Profile.token`'s own documentation allows the alternative: "an OIDC access token
/// **or** an API key". So the webview hands its access token down instead, on the
/// same two seams that persist the refresh token (`web/src/auth/session.ts`), and
/// this slot is what makes a token arriving at any moment reach a loop that started
/// without one.
///
/// **In memory only, deliberately.** Nothing here writes to `config.toml`: the
/// access token lives fifteen minutes, and persisting it would hand the CLI a
/// credential that works until lunchtime and then does not.
#[cfg(desktop)]
#[derive(Clone, Default)]
struct Credential(Arc<Mutex<SignedIn>>);

/// What the window last told the Rust half about who is using it.
///
/// The name rides along with the token rather than arriving on a wire of its own,
/// because it answers the same question at the same moment: this is who signed in,
/// and this is what to run as. Two calls would let them disagree.
#[cfg(desktop)]
#[derive(Default)]
struct SignedIn {
    session: Option<Session>,
    /// For the tray's header line. Presentation only — never used to authorize.
    user: Option<String>,
}

#[cfg(desktop)]
impl Credential {
    /// The current session, if there is one.
    ///
    /// A clone rather than a guard: the callers hold it across `.await` points, and
    /// a `MutexGuard` there would either not compile or deadlock the next writer.
    /// A poisoned lock reads as "no session" — the tray then shows `Disconnected`,
    /// which is true and is the one state this icon is never wrong to be in.
    fn session(&self) -> Option<Session> {
        self.0.lock().ok().and_then(|slot| slot.session.clone())
    }

    fn user(&self) -> Option<String> {
        self.0.lock().ok().and_then(|slot| slot.user.clone())
    }

    fn set(&self, session: Option<Session>, user: Option<String>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = SignedIn { session, user };
        }
    }
}

/// The tray's header line: the app, and who it is signed in as.
///
/// The app name alone when nobody is — not "weebo-notify — déconnecté", which would
/// spell in the header what the status line right below already says, and say it in
/// a place that is meant to be stable.
///
/// This is a menu item and not the tray tooltip on purpose: `tray-icon` documents
/// `set_tooltip` as **unsupported on Linux**, which is the platform this shell is
/// built and run on today, so a tooltip would be invisible exactly where it is
/// wanted. `set_title` does show there, but it prints into the panel permanently
/// and is unsupported on Windows. The menu is the one surface all three platforms
/// render.
#[cfg(desktop)]
fn identity_line(app_name: &str, user: Option<&str>) -> String {
    match user {
        Some(user) if !user.is_empty() => format!("{app_name} — {user}"),
        _ => app_name.to_owned(),
    }
}

/// Take the credential the webview just obtained, or drop it on sign-out.
///
/// Called from `session.ts`'s `remember`/`forget`, so the tray follows the window
/// within one poll rather than at the next launch.
#[cfg(desktop)]
#[tauri::command]
fn credential_session(
    app: AppHandle,
    token: Option<String>,
    name: Option<String>,
) -> Result<(), String> {
    let Some(credential) = app.try_state::<Credential>() else {
        return Err("the shell has no credential slot".to_owned());
    };

    let Some(token) = token else {
        credential.set(None, None);
        return Ok(());
    };

    // The profile is reloaded rather than captured: it carries the server URL, and
    // a token must never be paired with a stale one — that is the same reasoning
    // `notify login` states when it keeps the existing `url` and changes only the
    // credential.
    let mut profile = Profile::load(DEFAULT_PROFILE).map_err(|error| error.to_string())?;
    profile.token = Some(token);
    let session = Session::open(&profile).map_err(|error| error.to_string())?;
    credential.set(Some(session), name);
    Ok(())
}

/// How often the tray re-reads the server. See the module doc: this stands in for
/// real push until `notify-client-core` grows a stream consumer, and is a plain
/// judgment call rather than derived from any documented rate limit (§8.2 bounds
/// SSE connections, not `/unread` or `/preferences/dnd`).
#[cfg(desktop)]
const POLL: Duration = Duration::from_secs(20);

/// Build and run the app. Called from `main()` on desktop; called by the mobile
/// runtime on Android/iOS once a build for either exists (see the module doc — it
/// does not yet, D33/M6).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    // Relaunching focuses the window that already exists (§10bis.3): a second
    // process would hold a second SSE stream against §8.2's ceiling and pop
    // every notification twice.
    //
    // Desktop only, and not a capability being dropped: `tauri-plugin-single-instance`
    // has no `init` at all on Android and iOS, because the question does not arise
    // there. The OS runs one instance of an app, and the `android:launchMode="singleTask"`
    // that `cargo tauri android init` writes into the manifest already routes a
    // second launch back to the first activity rather than starting another.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
        focus_main(app);
        // On Windows and Linux a deep link reaches the *second* launch as an
        // argument, which is why it is read here as well as in the handler
        // below — the same click arrives by two different roads per platform.
        if let Some(link) = argv.iter().skip(1).find_map(|arg| parse_deep_link(arg)) {
            navigate(app, &link);
        }
    }));

    // What the login needs (`oauth`) and what makes it outlive the process
    // (`credential`), registered on the same `#[cfg(desktop)]` terms as the modules
    // they come from.
    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        oauth::loopback_authorize,
        oauth::open_external,
        credential::credential_save,
        credential::credential_load,
        credential::credential_clear,
        credential_session
    ]);

    let context = tauri::generate_context!();

    // The updater only when this build was given somewhere to update *from* and a
    // key to check it with — `plugins.updater` in the config, injected at build
    // time (README.md « Updates and signing »). Its `Config` has no default for
    // `pubkey`, so registering it unconditionally would make every dev and
    // unsigned build fail at startup rather than merely not update.
    #[cfg(desktop)]
    let builder = if context.config().plugins.0.contains_key("updater") {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };

    builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init())
        // Opens URLs in the *system* browser. Used from Rust only (`oauth`), so it
        // needs no entry in `capabilities/`. The webview reaches it indirectly,
        // through `loopback_authorize` and `open_external`, which is why both of
        // those refuse anything but an http(s) URL (`oauth::web_url`).
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Check once per launch, install in the background, apply at the next
            // start: no forced restart under somebody mid-read, and no menu item
            // yet (README.md's known gaps). The signature is verified against the
            // configured `pubkey` before anything is installed — the plugin's job,
            // and the reason it is not registered at all without one (above).
            #[cfg(desktop)]
            if app.config().plugins.0.contains_key("updater") {
                let updater_handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tauri_plugin_updater::UpdaterExt as _;
                    let outcome = async {
                        let Some(update) = updater_handle.updater()?.check().await? else {
                            return Ok(None);
                        };
                        update.download_and_install(|_, _| {}, || {}).await?;
                        Ok::<_, tauri_plugin_updater::Error>(Some(update.version))
                    }
                    .await;
                    match outcome {
                        Ok(Some(version)) => eprintln!(
                            "weebo-notify: update {version} installed; it applies at the next launch"
                        ),
                        Ok(None) => {}
                        Err(error) => eprintln!("weebo-notify: update check failed: {error}"),
                    }
                });
            }

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

            // Visible from the start, on every platform.
            //
            // This said `.visible(cfg!(mobile))` — hidden on desktop, because the
            // app's normal mode of life is the tray — and then, briefly, that the
            // window should appear when `profile.token` was absent. That second
            // version was wrong in a way worth recording rather than quietly
            // deleting: `Profile.token` is the *CLI's* credential, written by
            // `notify login` or read from `WNK_TOKEN` (`notify-client-core`'s
            // `config.rs`), and nothing in this crate ever writes it. The webview
            // signs in on its own and keeps its refresh token in `sessionStorage`
            // (`web/src/auth/session.ts`), which does not outlive the process.
            //
            // So the two states were inverted for the people who have either one:
            // sign in through the shell and the next launch still has no
            // `Profile.token`, so the window came up anyway; run `notify login` and
            // the window stayed hidden while the webview behind it had no session
            // at all — landing invisibly on the login screen, which is the exact
            // failure this was meant to remove.
            //
            // With no session that survives the process, the shell always starts on
            // the login screen, so the honest test is a constant. Making it
            // conditional again is a real option and a worthwhile one — it needs the
            // shell to persist its own credential first, the way `notify login`
            // does through `Profile::save_token`. That is a feature, not a flag.
            //
            // `background_color` is what keeps §10bis.3bis's "no white flash" true.
            // The window used to be shown only after the first paint; now that it is
            // up before it on every launch rather than never, the colour underneath
            // stopped being theoretical. This is `index.html`'s `theme-color` — the
            // dark theme's `--background` — and has to be updated with it.
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("weebo-notify")
                .visible(true)
                .background_color(Color(3, 0, 1, 255))
                .build()?;

            // Close hides rather than quits (§10bis.3ter): the app's normal mode of
            // life is the tray. The Linux "no tray host detected, so close really
            // quits" fallback the design also asks for is not implemented yet — on
            // a machine where the tray build below fails, this makes the window
            // reachable only by killing the process, a named cost rather than a
            // silent one.
            //
            // Desktop only for the same reason the window starts visible there:
            // preventing the close of the one window on a device with no tray does
            // not hide the app, it traps the person in it.
            #[cfg(desktop)]
            {
                let close_window = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = close_window.hide();
                    }
                });
            }

            // Nothing below this point runs on mobile — the tray, the menu it hangs
            // off, the polling that keeps it honest and the daemon that warms the
            // shared store are all desktop surface, and the store in particular is
            // warmed *for* the CLI and the TUI, neither of which exists on a phone.
            // Binding `window` keeps its `Drop` out of this block's tail on the
            // platforms that do not use it again.
            #[cfg(mobile)]
            let _ = window;

            #[cfg(desktop)]
            {
                // The synchronous half of startup, done on the main thread before
                // anything async runs, so the tray's very first paint reflects the
                // real (`Disconnected`) starting state rather than nothing at all —
                // §10bis.3bis's "no white flash" extended to the tray icon. Neither
                // call can fail in a way worth more than a line on stderr: a profile
                // is almost never absent (`Profile::load` falls back to a default),
                // and a session that fails to open just leaves the tray static.
                let profile = Profile::load(DEFAULT_PROFILE);
                let session = match &profile {
                    // Guarded on the token, and not merely on `open` succeeding:
                    // `Session::open` is happy without a credential — it just sends
                    // no `Authorization` header — so seeding the slot from a
                    // tokenless profile means handing the daemon a session certain
                    // to be refused. It reported exactly that, once, on every start
                    // before the window's own token arrived. Nothing was wrong; the
                    // line was just noise about a request nobody had asked for.
                    Ok(profile) if profile.token.is_some() => Session::open(profile).ok(),
                    Ok(_) => None,
                    Err(error) => {
                        eprintln!("weebo-notify: no usable profile: {error}");
                        None
                    }
                };

                let icons = TrayIcons::load();
                let initial = tray_now(0, false, false);

                // `package_info().name` rather than the string "weebo-notify":
                // `tauri.conf.json`'s `productName` is where that is decided, and a
                // second spelling here would be one to keep in step for no gain.
                let app_name = handle.package_info().name.clone();

                let identity_item =
                    MenuItemBuilder::with_id("identity", identity_line(&app_name, None))
                        .enabled(false)
                        .build(&handle)?;

                let status_item = MenuItemBuilder::with_id("status", &initial.status)
                    .enabled(false)
                    .build(&handle)?;

                let mut menu_builder = MenuBuilder::new(&handle)
                    .item(&identity_item)
                    .item(&status_item)
                    .separator()
                    .text("open-inbox", "Ouvrir l'inbox");

                // Unconditional now, where it used to be built only when startup
                // already had a session. That condition was right when the answer
                // could not change — a menu is built once, so omitting the submenu
                // was the only way to avoid a dead affordance. It stopped being
                // right when the credential became something that arrives later:
                // the alternative is a menu that never grows the entry for the rest
                // of the run, which is a *permanently* dead affordance rather than a
                // briefly inert one.
                let dnd_menu = SubmenuBuilder::new(&handle, "Ne pas déranger")
                    .text("dnd-1h", "1h")
                    .text("dnd-until-morning", "jusqu'à demain 8h")
                    .text("dnd-off", "désactiver")
                    .build()?;
                menu_builder = menu_builder.separator().item(&dnd_menu);

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

                // The slot starts holding whatever `notify login` left on this
                // machine, which may well be nothing — that is no longer a reason
                // not to start the loops below. Only a token in the config file or
                // `WNK_TOKEN`: `notify login` now prefers the OS vault, under the
                // CLI's own entry (`crates/notify-cli/src/login.rs`), which this
                // does not read — the window's own sign-in fills the slot anyway.
                let credential = Credential::default();
                // No name to go with it: a token from `notify login` says what to
                // run as, never who. The header stays at the app name until the
                // window signs in and tells us.
                credential.set(session, None);

                match tray_result {
                    Ok(tray_icon) => {
                        // Managed so `on_menu_event` — which only ever sees an
                        // `AppHandle`, never this closure's locals — can reach it for
                        // the DND quick actions, and so `credential_session` can put
                        // the window's own token into it.
                        app.manage(credential.clone());

                        if let Some(daemon) = open_daemon() {
                            let daemon_session = credential.clone();
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
                            credential,
                            icons,
                            tray_icon,
                            status_item,
                            identity_item,
                            app_name,
                            initial,
                        ));
                    }
                    // No tray host on this machine — real on Linux even after
                    // `task tauri:doctor` is clean: the appindicator backend `dlopen`s
                    // its library at *runtime*, with no build-time probe. The window
                    // still works; this run is then an ordinary windowed app, minus
                    // the close-to-hide affordance's way back in (see above).
                    Err(error) => {
                        eprintln!("weebo-notify: tray unavailable: {error}");
                    }
                }
            }

            Ok(())
        })
        .run(context)
        .expect("the shell failed to start");
}

/// The four committed tray glyphs (`icons/tray-*.png`), decoded once at startup.
///
/// `include_bytes!` fails the *build* if a file goes missing — the same "a check
/// that runs is never out of date" instinct the icons' own README already applies
/// to their rasterisation. A `PNG` that exists but fails to decode is a build-time
/// mistake in an already-committed, already-rasterised asset, not a runtime
/// condition, so [`TrayIcons::load`] expects rather than handles it.
#[cfg(desktop)]
struct TrayIcons {
    neutral: Image<'static>,
    unread: Image<'static>,
    dnd: Image<'static>,
    offline: Image<'static>,
}

#[cfg(desktop)]
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
#[cfg(desktop)]
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
#[cfg(desktop)]
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
#[cfg(desktop)]
async fn poll_loop(
    credential: Credential,
    icons: TrayIcons,
    tray_icon: TrayIcon,
    status_item: MenuItem<tauri::Wry>,
    identity_item: MenuItem<tauri::Wry>,
    app_name: String,
    initial: Tray,
) {
    let mut interval = tokio::time::interval(POLL);
    let mut last_state = initial;
    let mut last_unread = 0u64;
    let mut last_quiet = false;
    let mut last_identity = identity_line(&app_name, None);

    loop {
        // Tokio's default `MissedTickBehavior` fires the first tick immediately,
        // so the tray's very first live read happens right after the static
        // `Disconnected` frame built in `setup`, not up to `POLL` later.
        interval.tick().await;

        // The header first, and *before* the early return below: signing out has to
        // take the name off the menu, and that is exactly the tick on which there is
        // no session left to poll.
        let identity = identity_line(&app_name, credential.user().as_deref());
        if identity != last_identity {
            let _ = identity_item.set_text(&identity);
            last_identity = identity;
        }

        // Read per tick rather than once before the loop: the whole point of the
        // slot is that a sign-in in the window reaches this loop without a restart.
        // No credential is not an error state to report, it is `Disconnected` —
        // which is what the icon already shows, so there is nothing to redraw.
        let Some(session) = credential.session() else {
            continue;
        };

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
///
/// Takes the slot rather than a `Session` for the same reason [`poll_loop`] does —
/// and with one failure of its own to name: before this, a shell started without a
/// credential printed `sync: the credential presented is not valid` every `POLL`,
/// forever, because it had opened a session against a profile with no token and
/// kept using it. Reading the slot per tick means "not signed in yet" is silence,
/// which is what it should always have been.
#[cfg(desktop)]
fn run_daemon(credential: Credential, mut daemon: Daemon) {
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

            let Some(session) = credential.session() else {
                continue;
            };

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
#[cfg(desktop)]
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "open-inbox" => focus_main(app),
        "quit" => app.exit(0),
        id @ ("dnd-1h" | "dnd-until-morning" | "dnd-off") => {
            // Through the slot, so this follows a sign-in that happened after
            // startup. Returning on absence is unchanged and still right: the menu
            // entry now exists before there is a session to write through, and a
            // click in that window has nowhere to go — silently, because §10bis.3ter
            // rules out an error popup from this icon.
            let Some(session) = app
                .try_state::<Credential>()
                .and_then(|credential| credential.session())
            else {
                return;
            };
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
#[cfg(desktop)]
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
        //
        // Desktop only because Tauri has no `unminimize` on mobile — not an
        // omission on their part, there is nothing to un-minimise: an Android
        // activity is either resumed or it is not, and `show`/`set_focus` below
        // (which do exist on both) are the whole of the operation there.
        #[cfg(desktop)]
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
#[cfg(desktop)]
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

#[cfg(all(test, desktop))]
mod tests {
    use super::identity_line;

    /// The header says who, and stays useful when nobody has said so yet.
    #[test]
    fn the_header_names_the_app_alone_until_somebody_signs_in() {
        assert_eq!(identity_line("weebo-notify", None), "weebo-notify");
        assert_eq!(
            identity_line("weebo-notify", Some("Alice Martin")),
            "weebo-notify — Alice Martin",
        );
    }

    /// A provider that returns a blank `name` is not a person called "".
    ///
    /// `displayName` on the web side falls back through name → email → sub and ends
    /// at the empty string, so this is the shape that actually arrives — and an
    /// em-dash trailing into nothing is worse than no dash at all.
    #[test]
    fn an_empty_name_reads_as_no_name() {
        assert_eq!(identity_line("weebo-notify", Some("")), "weebo-notify");
    }
}
