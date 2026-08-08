# The desktop shell (design §10bis.3, milestone M5)

**This directory now builds.** It is still outside the cargo workspace on purpose —
see the note at the top of `Cargo.toml` — because on Linux Tauri links against a
GTK/WebKit stack that `mise` cannot install and a rootless workspace cannot
`apt-get` (§12.4, D13). `task tauri:doctor` says whether *this* machine can build
it; once it says yes, `task tauri:build`/`:dev`/`:check` do the rest.

The crate is split into `lib.rs` (everything that actually happens) and a thin
`main.rs` (desktop's `fn main`, which just calls `lib.rs::run`) — the shape
Tauri's own `cargo tauri android/ios init` expects to find, since mobile links
this crate as a `staticlib`/`cdylib` and calls `run` via
`tauri::mobile_entry_point`, never a `fn main`. That is the entire M6 (Android,
§10bis.4) content of this increment: no platform code, no Android build — this
workspace has no SDK/NDK to build one with yet (§14.4 D33,
`docs/upstream/che-min-mise-android.md`) — just the crate shape that command
needs before it can do anything.

## What is tested, and where

The half that does not need a webview lives in `notify-client-core` and runs under
`task test` on any machine:

| Concern | Module | Design |
|---|---|---|
| Tray icon state machine, menu status line | `notify_client_core::tray` | §10bis.3ter |
| `weebo-notify://n/<id>` and `//c/<id>` | `notify_client_core::deeplink` | §10bis.3quater |

That split is not tidiness. §10bis.3ter ends on the rule that the tray counter, the
dock badge and the web sidebar read the **same state** — "trois rendus, une seule
vérité" — so a state machine written in this crate would be a fourth truth, and the
only one nobody could test without a desktop session.

## Icons

Authored as SVG in `icons/src/`, rasterised by `task gen` (`xtask icons`). Both the
sources and the PNGs are committed: a PNG alone would arrive in review as "binary
files differ", which is the one diff a design decision must not hide.

The tray marks use a **cropped viewBox** (`96 108 320 320`) rather than the app
icon's full `0 0 512 512`. That is the whole difference between legible and mush at
32 px, and it was found by rendering them and looking: the app icon is drawn for a
maskable plate, so it keeps a wide margin an OS is allowed to crop, and inheriting
that margin into a tray glyph left the mark filling about 45 % of the output.

`icons/icon.png` is the only bundle icon here, which is why `bundle.targets` is
`deb` and `appimage` only. Windows (`nsis`) wants a `.ico` and macOS (`app`, `dmg`)
a `.icns`; `pnpm tauri icon icons/icon@1024.png` generates the whole platform set
from the large PNG, and that step belongs to whoever first builds on those
platforms rather than to a guess made here.

## Known gaps in this scaffold

Beyond the §10bis bullets that are simply not started (see §14.1, M5) — §10bis.3bis's
hygiene now lives in the shared front, gated on `is-desktop`:

- the tray **is** built (`TrayIconBuilder`, a real menu, DND quick actions,
  close-to-hide) and driven by `notify-client-core`'s `Session`/`Store`/election, on
  the same pattern the TUI uses (`crates/notify-cli/src/tui/mod.rs::run`);
- the tray's state is **polled** (every 20s), not pushed — `notify-client-core` has
  no SSE/stream consumer at all yet, only the pull-based `GET /sync`. "Temps réel"
  (§10bis.3) is not delivered by this increment, and is arguably the more
  consequential gap the rest of the desktop story now sits on top of;
- the tray's `quiet` reading reflects only the quick DND toggle (`Dnd.until`), never
  a recurring `quiet_hours` schedule — this crate deliberately does not depend on
  `notify-domain`'s `QuietHours::covers`, and re-deriving that logic here would
  repeat D22 (the exact mistake `dnd.rs`'s own module doc names);
- the dock/taskbar badge (§10bis.3bis) and the quick panel window
  (`tauri-plugin-positioner`, §10bis.3quater) are still unbuilt — the tray is the
  first of the "trois rendus," not the third;
- no updater, no signing, no `beta` channel — and no "Rechercher les mises à jour"
  menu item either, on purpose: an inert item with nothing to check against would be
  the same "port implemented, called by nobody" mistake D27 already named once;
- no Linux tray-detection at launch — closing the window always hides, even on a
  machine where the tray itself failed to build (real, at runtime, even past
  `task tauri:doctor` — see `lib.rs`'s note on the appindicator backend);
- no profile switcher — the shell always opens `notify_client_core::DEFAULT_PROFILE`;
- no global shortcut (`tauri-plugin-global-shortcut`), no CLI sidecar packaging;
- no Android/iOS build of any kind (M6/M7) — `lib.rs`/`main.rs` are shaped for
  `cargo tauri android/ios init`, but nobody has run it and nothing here has ever
  targeted `aarch64-linux-android` or anything else mobile; there is no
  `gen/android` or `gen/apple` in this repository.
