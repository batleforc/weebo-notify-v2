# Upstream — the Android toolchain: withdrawn, nothing to send (M6)

Status: **withdrawn 2026-08-10.** No change to `che-min-mise` is needed for Android.
The toolchain landed in this repository instead — `mise.android.toml`,
`.tasks/android.yaml` and a devfile volume.

This file used to be a proposal to bake a ~2–3 GB Android SDK/NDK into the dev image
with a root `sdkmanager` script. It is kept rather than deleted because the version
choices in it are still the right ones and are still in force, and because "why did
nobody send this upstream" is a question worth having an answer to.

## The premise that was wrong

The proposal rested on one sentence:

> `mise` has no first-class Android SDK backend, which is exactly why this diff is a
> `sdkmanager` script and not a `mise use`.

It has one. Verified in a Che `tools` container, as the ordinary non-root user, with
no `apt` and no image change:

| Claim | Result |
|---|---|
| `mise registry` carries an Android SDK backend | `android-sdk → vfox:mise-plugins/vfox-android-sdk` |
| It installs without root | yes — 150 MB into `$MISE_DATA_DIR` |
| It sets what Gradle and Tauri look for | `ANDROID_HOME`, `ANDROID_SDK_ROOT`, `cmdline-tools/<v>/bin` on `PATH`, automatically |
| `sdkmanager` runs | once `JAVA_HOME` exists — which mise's `java` backend also sets, and a distro JDK package does not |
| Licences accepted without root | `sdkmanager --sdk_root=… --licenses` → exit 0, writes `licenses/` |
| Components install to a **separate** root | `sdkmanager --sdk_root=… platform-tools` → exit 0, 22 MB |

The last two rows are the ones that decided it. Nothing here needs root, so nothing
here needs an image.

The design document was right all along: §10bis.6's *"Android en conteneur (SDK/NDK
pinnés via mise)"* is exactly what this does. The correction this file used to
propose to that line should **not** be applied.

## What landed instead

**`mise.android.toml`** — `java = "temurin-17"` and the Android command-line tools,
loaded only under `MISE_ENV=android`. A third audience alongside `mise.toml` (what
the gates need) and `mise.dev.toml` (what a developer wants). `MISE_ENV` composes,
so `MISE_ENV=dev,android mise install` gets both.

Not covered by `mise.lock` — mise locks `mise.toml` only, verified — so both
versions are written explicitly rather than as `latest`.

**`.tasks/android.yaml`** — `android:sdk` installs the components below into a
persistent root and adds the four Rust targets; `android:doctor` reports what is
missing without failing, shaped on `task tauri:doctor`.

**`devfile.yaml`** — a 6 Gi `android-sdk` volume mounted at `/home/user/.android-sdk`
on the inherited `tools` container, with `ANDROID_HOME` and `ANDROID_SDK_ROOT` set
to it. Both names, because Gradle reads the first and newer Google tooling prefers
the second, and setting one is how you get a build that works for one tool and not
the other.

They point at that path rather than at mise's install directory — which the plugin
would otherwise set them to — because mise owns that directory and replaces it whole
on a version bump. `sdkmanager` is always called with `--sdk_root` to match.

## The component versions, unchanged

Still the right values, now pinned in `.tasks/android.yaml` rather than in an image:

| Component | Pinned | Why |
|---|---|---|
| `platform-tools` | rolling | `adb`/`fastboot`. Nothing links against them; only needed if a device is ever attached to this pod. |
| `platforms;android-35` | Android 15 | What `cargo tauri android init` targets by default. Bump in lockstep with whatever `gen/android/build.gradle.kts` ends up pinning, once that directory exists. |
| `build-tools;35.0.0` | matches the platform | `apksigner`, `aapt2` — the packaging toolchain. |
| `ndk;27.2.12479018` | NDK 27, side-by-side | Provides `clang` per Android target triple. This is what actually links `notify-client-core`: `rusqlite`'s `bundled` feature already means "building a client needs a C compiler", and this is that compiler, cross-targeted. |
| JDK 17 | `temurin-17` via mise | Android Gradle Plugin 8.x requires 17 or newer. |

`gradle` and `kotlin` are deliberately absent: `cargo tauri android init` generates a
Gradle *wrapper*, and the Kotlin plugin version comes from that generated build. A
system copy of either would be a second version silently disagreeing with the pinned
one.

## What changed about licence acceptance, and why it is better

The withdrawn proposal put `sdkmanager --licenses` in a root `RUN` block, and said so
plainly:

> license acceptance is a build-time, root, one-organisation decision

That was the honest reading of a plan that had no other option. Now there is one:
acceptance runs in `task android:sdk`, as the person doing the building, writing its
record to `{{.SDK_ROOT}}/licenses` in their own workspace. "Who agreed to what" has an
answer again, which the image-wide version could never give.

## The cost, compared

| | Withdrawn proposal | What landed |
|---|---|---|
| Image growth | +2–3 GB, paid by **every** workspace | none |
| Root required | yes | no |
| Who accepts the licence | the image build, for everyone | whoever runs `task android:sdk` |
| Where versions are pinned | `versions.yaml`, another repository | `mise.android.toml` and `.tasks/android.yaml`, here |
| Cost to an Android builder | none extra | ~3 GB into a volume, once |
| Upstream change | a new `RUN` and `ENV` block | **none** |

The one genuine regression: a fresh workspace cannot build Android immediately — it
needs `MISE_ENV=dev,android mise install` and then `task android:sdk`. That is the
same trade `task init` and `task dev:up` already make, and `task android:doctor` is
what tells somebody they need it.

## What this still does not close

Unchanged, and worth repeating so this is not read as more than it is:

- M6's Kotlin work — Android Keystore enrolment and the embedded UnifiedPush distributor — is not written, and no toolchain change writes it.
- **D32**, the inbound push endpoint, stays open on purpose: writing that route before the Kotlin client exists is guessing an API against a client nobody has written.
- `web/src-tauri/gen/android` does not exist until somebody runs `cargo tauri android init`. `task android:doctor` checks the toolchain, not the project, precisely so it is useful before that.

What changed is only that none of it is blocked on somebody else's image release.

## Checking it

```bash
MISE_ENV=dev,android mise install    # ~150 MB: JDK + command-line tools
task android:sdk                     # ~3 GB: platform, build-tools, NDK, rust targets
task android:doctor                  # reports; never fails
task tauri:doctor                    # the desktop half — a separate question
```

## The general rule this made concrete

Worth stating, because it will apply again: **if `mise` can carry it, it belongs in a
`mise.*.toml`; the image is for what needs root.**

That is what turned a 3 GB image change into a `mise use`. The desktop shell's
GTK/WebKit stack is the counter-example that proves the rule — mise carries tools,
not C libraries — and it is the one thing that did have to go upstream. That fix has
landed: this repository's devfile parent is `che-mise-webkit`, and `task
tauri:doctor` reports all four `pkg-config` names present. See
[`che-min-mise-tauri.md`](./che-min-mise-tauri.md), which is likewise now a record
rather than a pending request.
