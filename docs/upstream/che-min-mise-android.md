# Upstream — the Android SDK/NDK toolchain in `che-min-mise` (M6)

This is a proposed fix, not yet the real one — see "What this is not," below. It does
not belong in this repository, for the same reason the webkit fix
(`che-min-mise-tauri.md`) does not: design §3 says a generic improvement goes back
into the template rather than being worked around in a consumer, and §12.4 already
says as much for the whole "tools container has no root" class of problem.

What lives here is a diff shaped closely enough on the webkit one to apply
directly, plus how to check it landed and what to delete in this repository
afterwards — written *before* anybody needs it, the way the webkit diff was ready
in `docs/upstream/che-min-mise-tauri.md` before `che-mise-webkit` existed.

## What this is not

The webkit fix closed D13 outright: once the four libraries were in the image,
`web/src-tauri` became a member of what `task tauri:build` could actually attempt.
This one does not close anything by itself. M6's own status line
(`docs/.prompt/weebo-notify-design.md`, milestone table) is explicit that the
Android Keystore enrollment and the embedded UnifiedPush distributor are Kotlin,
and D32 — the inbound push endpoint the server is supposed to expose — is left
**open on purpose**, because writing that route before the Kotlin client exists
would be guessing an API against a client nobody has written yet. Landing this
image change makes the Kotlin side buildable; it does not write it.

## Why this is a different *shape* of fix than webkit's

Same root cause — the `tools` container's pod-wide non-root security context, so
nothing in it can `apt-get` at runtime (§12.4) — but a different remedy, because not
everything Tauri Android needs is an `apt` package:

- **The JDK is** an ordinary system package (Ubuntu carries `openjdk-17-jdk`), so it
  can join the same "Core packages" `apt-get` block the webkit fix already
  extended.
- **The Android SDK and NDK are not.** Google distributes them as a licensed,
  version-pinned zip (`commandlinetools-linux-*.zip`) fetched by its own
  `sdkmanager`, with a license users must accept — there is no Debian/Ubuntu
  package for `platforms;android-35` or `ndk;27.2.12479018`. That has to be a
  scripted download-and-`sdkmanager`-install step in the image build, run once, as
  root, with the license accepted non-interactively — not a list of names appended
  to an existing `apt-get install` line.

So this diff has two parts instead of one: a small `apt` addition, and a new,
self-contained `RUN` block for the SDK/NDK.

## The diff

Target: `che-min-mise/Containerfile.tpl`, the same Ubuntu 26.04 base the webkit fix
already lands on.

**1. The JDK**, joining the same "Core packages" block webkit's fix extends (Android
Gradle Plugin 8.x — the version `cargo tauri android init` scaffolds — requires
JDK 17 or newer; 17 is the current LTS and what the SDK's own tooling is tested
against):

```diff
     libayatana-appindicator3-dev \
     librsvg2-dev \
     patchelf \
+    openjdk-17-jdk-headless \
     pkg-config &&\
     rm -rf /var/lib/apt/lists/* &&\
```

**2. The SDK and NDK**, a new block after the core package layer:

```dockerfile
# Android SDK/NDK for Tauri v2 mobile (M6, design §10bis.4). Not an apt package —
# Google ships the command-line tools as a licensed zip, installed by sdkmanager.
# Versions pinned here the same way the rest of this image pins them (versions.yaml)
# rather than tracking "latest", which moves under a build nobody asked to change.
ENV ANDROID_HOME=/opt/android-sdk
ENV NDK_VERSION=27.2.12479018
ENV ANDROID_NDK_HOME=${ANDROID_HOME}/ndk/${NDK_VERSION}
ENV JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64
ENV PATH=${PATH}:${ANDROID_HOME}/cmdline-tools/latest/bin:${ANDROID_HOME}/platform-tools

RUN CMDLINE_TOOLS_VERSION=13114758 &&\
    mkdir -p ${ANDROID_HOME}/cmdline-tools &&\
    curl -fsSL -o /tmp/cmdline-tools.zip \
      "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS_VERSION}_latest.zip" &&\
    unzip -q /tmp/cmdline-tools.zip -d ${ANDROID_HOME}/cmdline-tools &&\
    mv ${ANDROID_HOME}/cmdline-tools/cmdline-tools ${ANDROID_HOME}/cmdline-tools/latest &&\
    rm /tmp/cmdline-tools.zip &&\
    yes | sdkmanager --licenses > /dev/null &&\
    sdkmanager \
      "platform-tools" \
      "platforms;android-35" \
      "build-tools;35.0.0" \
      "ndk;${NDK_VERSION}"
```

| Component | Version pinned here | Why |
|---|---|---|
| `openjdk-17-jdk-headless` | Ubuntu 26.04's | Runs Gradle, which `cargo tauri android build` drives; `-headless` because no display server exists in this pod. |
| `platform-tools` | latest (rolling, `adb`/`fastboot` don't gate a build) | Not linked against; only needed for `adb` if anyone ever attaches a device to this pod, which nothing here currently plans to do. |
| `platforms;android-35` | Android 15 | The compile-against SDK `cargo tauri android init` targets by default at time of writing; bump in lockstep with whatever `web/src-tauri/gen/android/build.gradle.kts` ends up pinning once that directory exists. |
| `build-tools;35.0.0` | matches the platform | `apksigner`, `aapt2` and friends — the actual packaging toolchain. |
| `ndk;27.2.12479018` | NDK 27, side-by-side | Provides `clang` for each Android target triple — this is what actually links `notify-client-core` for Android. `rusqlite`'s `bundled` feature already means "building a client needs a C compiler" (`crates/notify-client-core/src/store.rs`'s own module doc); this is that compiler, cross-targeted. |

**3. The Rust cross targets** — not a Containerfile change, since `rustup` already
lives in this image; a one-line addition wherever the image's Rust setup step is:

```diff
     rustup component add clippy rustfmt &&\
+    rustup target add aarch64-linux-android armv7-linux-androideabi \
+      i686-linux-android x86_64-linux-android &&\
```

## The cost, stated plainly

The SDK platform, build-tools and one NDK version together add on the order of two
to three gigabytes to the image — bigger than the whole webkit fix, whose seven
`-dev` packages are headers and small shared libraries. That is worth weighing
against the same storage note this project's Che guidance already gives for
`target/`/`node_modules`: an image this much larger is slower to pull on every fresh
workspace, not only on the one someone builds Android from.

## Checking it landed

No `task android:doctor` exists yet — there is nothing in this repository to check
it against, since `web/src-tauri/gen/android` is not generated until somebody runs
`cargo tauri android init` against a landed image. Once this diff is live, the
natural next step is a doctor task shaped exactly like `task tauri:doctor`
(`.tasks/tauri.yaml`): report `ANDROID_HOME`, `ANDROID_NDK_HOME`, `javac`, and the
four `rustup target list --installed` entries, fail nothing, print what's missing.
Not written here, on purpose — the same "no dead affordance" reasoning M5 applied
to menu items applies to a task nothing in this repository can run yet.

## Why there is no workaround in this repository

Same conclusion as webkit's, for the same first reason — a sidecar cannot `apt-get`
or unzip into a root-owned `/opt` any more than `tools` can, because the pod's
security context is shared — plus one reason specific to this fix: **license
acceptance is a build-time, root, one-organisation decision**, not something a
workspace should paper over per-session. `sdkmanager --licenses` writes an
acceptance record Google's tooling checks before it will use the SDK at all; baking
that into the image once, deliberately, is the honest place for it. A sidecar or a
per-workspace script that re-accepts it silently on every start is the kind of
thing that reads fine until someone asks who agreed to what.

## What to update here afterwards

1. A new decision in §14.4, mirroring D13's shape: "the dev image does not carry
   the Android SDK/NDK toolchain" — opened here rather than left implicit, closed
   the tag this lands in.
2. `.tasks/android.yaml` and a `task android:doctor`, once there is something in
   this repository (`gen/android`) for it to check against.
3. §10bis.6's CI line ("Android en conteneur (SDK/NDK pinnés via mise)") is wrong in
   one detail once this lands: the SDK/NDK are pinned in the *image*, not via
   `mise` — `mise` has no first-class Android SDK backend, which is exactly why this
   diff is a `sdkmanager` script and not a `mise use`. Worth a one-line correction
   there rather than leaving a design doc that names a mechanism this fix doesn't
   use.

Nothing else in this repository changes on its own: like the webkit fix, the
`tools` container inherits from the parent, so a rebuilt tag is most of it — the
rest is the Kotlin work (Android Keystore enrollment, the embedded distributor) and
D32, both still ahead of this and both still open regardless of what the image
carries.
