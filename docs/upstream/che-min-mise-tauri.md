# Upstream — the GTK/WebKit stack in `che-min-mise` (D13)

This is the real fix for D13, and it does not belong in this repository. Design §3
says a generic improvement goes back into the template rather than being worked
around in a consumer, and §12.4 already says it in so many words: *"le correctif
appartient donc à l'image, c'est-à-dire à `batleforc/WeeboDevImage`"*.

What lives here is only the diff, ready to apply, plus how to check it worked and
what to delete in this repository afterwards.

## Why

Tauri v2 links on Linux against a GTK/WebKit stack that:

- `mise` cannot ship — it carries tools, not C libraries;
- cannot be installed at runtime — the `tools` container has no root, by design.

So the shell cannot be built at all in a Che workspace, which is not "M5 fails to
build" but something wider: **nothing in M5 can be verified here, not even a
`cargo check`.** That is why M5 has never been started rather than started badly.

Verify the current state with `task tauri:doctor`, which reports rather than fails
and lists exactly what is missing.

## The diff

Target: `che-min-mise/Containerfile.tpl`, the "Core packages" block — the one that
already ends with `libdbus-1-dev` and `pkg-config`, so seven names join a list that
exists rather than opening a new layer.

```diff
     libdbus-1-dev \
+    libwebkit2gtk-4.1-dev \
+    libjavascriptcoregtk-4.1-dev \
+    libsoup-3.0-dev \
+    libgtk-3-dev \
+    libayatana-appindicator3-dev \
+    librsvg2-dev \
+    patchelf \
     pkg-config &&\
     rm -rf /var/lib/apt/lists/* &&\
```

What each one is for, so that nobody later removes one as unused:

| Package | Why |
|---|---|
| `libwebkit2gtk-4.1-dev` | The webview itself. |
| `libjavascriptcoregtk-4.1-dev` | Pulled in by the above, named explicitly because `pkg-config` is asked for it by name. |
| `libsoup-3.0-dev` | HTTP stack WebKitGTK links against. |
| `libgtk-3-dev` | The window, the menus, the tray host. |
| `libayatana-appindicator3-dev` | The tray (§10bis.3ter) — the app is an inbox daemon, so this is not optional decoration. |
| `librsvg2-dev` | Icon rendering during bundling. |
| `patchelf` | What the AppImage bundler calls. |

The base is Ubuntu 26.04 (`versions.yaml`, `base.ubuntu`). The `-dev` names above are
correct there: the `t64` suffix that 26.04 applies is on the runtime library
packages, not on their development counterparts.

## Checking it landed

In a workspace rebuilt on the new tag:

```bash
task tauri:doctor      # the four pkg-config names, all ok
```

That command is the check on purpose, rather than a paragraph: the answer differs
per machine and changes the moment an image is rebuilt, so a sentence written here
would be wrong the day somebody fixes their environment.

## Why there is no workaround in this repository

One was written and removed, and the reason is worth keeping so nobody spends the
afternoon again: **a sidecar does not help.** A DevWorkspace applies its security
context to the whole pod, so no container in it runs as root — `apt-get` fails in a
new sidecar exactly as it fails in `tools`. Scheduling a container and being root
inside one are different capabilities, and this needs the second.

A prebuilt image that already carries the stack would sidestep the root question,
and it trades it for a worse one: a second Rust toolchain, a second `target/`
directory, and a build environment that is not the one anybody ships from. The
seven packages belong where every other system dependency already is.

## What to delete here afterwards

1. The paragraph in §12.4 that says the image does not carry the stack, and the one
   after it explaining why no sidecar stands in for it.
2. D13 in §14.4, moved to soldée with the tag that carries it.

Nothing else in this repository changes: the `tools` container inherits from the
parent, so a rebuilt tag is the whole fix.
