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

## Updates and signing

`tauri-plugin-updater` is compiled in on desktop but only **registered** when the
build's config carries a `plugins.updater` section (`lib.rs::run`): the plugin
refuses to start without a `pubkey`, and a dev or unsigned build must still run.
Without that section the app simply never updates — nothing fails.

1. **Generate the key pair, once**, and keep the private half out of the repository:

   ```sh
   pnpm tauri signer generate -w ~/.tauri/weebo-notify.key
   ```

   It prints the public key (also written to `~/.tauri/weebo-notify.key.pub`).

2. **CI secrets.** Store the private key's *contents* as `TAURI_SIGNING_PRIVATE_KEY`
   and its password as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. `tauri build` signs the
   update artefacts with them and writes a `.sig` next to each bundle.

3. **Build with the environment set.** `task tauri:bundle` (and `task tauri:build`)
   pass `web/scripts/tauri-csp.ts --updater` to Tauri as a config merge patch, which
   reads (`src/lib/tauri-updater.ts`, tested in its `.spec.ts`):

   | Variable | Effect |
   |---|---|
   | `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | `bundle.createUpdaterArtifacts`: each artefact is signed and gets its `.sig`. Tauri reads both variables itself. |
   | `WNK_UPDATER_ENDPOINT` | `plugins.updater.endpoints` — must be `https://`, e.g. `https://<host>/releases/{{target}}-{{arch}}/latest.json`, serving the static manifest the plugin expects (`version`, `platforms.<target>.url` and `.signature` — the `.sig` contents). |
   | `WNK_UPDATER_PUBKEY` | `plugins.updater.pubkey` — the contents of `weebo-notify.key.pub`. |

   None set: an unsigned build with no updater, as before. An endpoint without a
   public key (or a key without an endpoint, or a plain-`http` endpoint) **fails the
   build** — an updater with nothing to verify against would install whatever the
   endpoint serves. Nothing is committed to `tauri.conf.json`, so a build without
   the secrets can never turn into one that updates.

**What the signature does not cover: the manifest.** minisign signs each bundle, not
`latest.json`, so the `version` the plugin compares against the installed one is a
label nobody signed. Whoever controls the endpoint can serve an *older*, genuinely
signed build labelled `9.9.9`, and every desktop installs it on its next launch
(a downgrade to a known-vulnerable release). Until the client checks the version
inside the bundle it downloaded, treat the endpoint as release infrastructure: serve
it from the release pipeline only, over HTTPS, from a host only that pipeline can
write, and generate each manifest from its own tag.

The signature checked here is the updater's own (minisign), and it is what makes an
update trustworthy. It is not OS code signing, which needs certificates this project
does not have — so it is not wired, only described. What Tauri expects when they
exist:

- **macOS** (signing + notarisation): `APPLE_CERTIFICATE` (base64 `.p12`),
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, then either `APPLE_ID` +
  `APPLE_PASSWORD` (app-specific) + `APPLE_TEAM_ID`, or `APPLE_API_ISSUER` +
  `APPLE_API_KEY` + `APPLE_API_KEY_PATH`. Needs a `dmg`/`app` target and `.icns`
  icons first (see « Icons »).
- **Windows** (Authenticode): no environment variable — `bundle.windows.certificateThumbprint`,
  `digestAlgorithm` and `timestampUrl` for a certificate in the runner's store, or
  `bundle.windows.signCommand` for a remote signer (Azure Trusted Signing, a HSM).
  Needs an `nsis`/`msi` target and a `.ico` first.
- **Linux** `deb`/`appimage`: nothing — they carry no OS-enforced signature.

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
- the updater checks once per launch and installs for the next start (see « Updates
  and signing » below), but there is no `beta` channel and no "Rechercher les mises à
  jour" menu item yet, and no published endpoint or key — the build is wired for
  them, nothing serves them; OS code signing (Authenticode, Apple notarisation) is
  not set up either;
- no Linux tray-detection at launch — closing the window always hides, even on a
  machine where the tray itself failed to build (real, at runtime, even past
  `task tauri:doctor` — see `lib.rs`'s note on the appindicator backend);
- no profile switcher — the shell always opens `notify_client_core::DEFAULT_PROFILE`;
- no global shortcut (`tauri-plugin-global-shortcut`), no CLI sidecar packaging;
- no Android/iOS build of any kind (M6/M7) — `lib.rs`/`main.rs` are shaped for
  `cargo tauri android/ios init`, but nobody has run it and nothing here has ever
  targeted `aarch64-linux-android` or anything else mobile; there is no
  `gen/android` or `gen/apple` in this repository.
