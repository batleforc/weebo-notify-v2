# The desktop shell (design §10bis.3, milestone M5)

**Nothing in this directory has ever been compiled.** It is outside the cargo
workspace on purpose — see the note at the top of `Cargo.toml` — because on Linux
Tauri links against a GTK/WebKit stack that `mise` cannot install and a rootless
workspace cannot `apt-get` (§12.4, D13). `task tauri:doctor` says whether *this*
machine can build it.

Read every line here as a proposal until the first build says otherwise.

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

- the tray is **not built** — `main.rs` maps a `Tray` onto an asset and stops there;
  no `TrayIconBuilder`, no menu, no quick panel;
- nothing reads `notify-client-core`'s session, cache or unread count yet, so the
  tray state machine has no live input in this process;
- no updater, no signing, no `beta` channel (§10bis.3, bullet 4).
