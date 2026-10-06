---
name: weebo-notify
description: A proof sheet for notifications — BatleHub's sibling world, where a notification's lifecycle is drawn as a pixel matrix.
colors:
  ground: "oklch(0.07 0.018 18)"
  ground-raised: "oklch(0.155 0.022 18)"
  ground-sunk: "oklch(0.045 0.014 18)"
  ink: "oklch(0.93 0.018 25)"
  ink-dim: "oklch(0.62 0.045 30)"
  rule-soft: "oklch(0.34 0.03 25)"
  rule-strong: "oklch(0.52 0.06 25)"
  signal: "oklch(0.65 0.235 25)"
  signal-ink: "oklch(0.1 0.02 18)"
  copper: "oklch(0.72 0.14 52)"
  verdigris: "oklch(0.76 0.13 160)"
  focus: "oklch(0.85 0.16 85)"
  light-ground: "oklch(0.97 0.008 18)"
  light-ground-raised: "oklch(0.935 0.01 18)"
  light-ground-sunk: "oklch(0.99 0.004 18)"
  light-ink: "oklch(0.2 0.022 20)"
  light-ink-dim: "oklch(0.44 0.04 25)"
  light-rule-soft: "oklch(0.8 0.02 25)"
  light-rule-strong: "oklch(0.62 0.05 25)"
  light-signal: "oklch(0.52 0.21 25)"
  light-signal-ink: "oklch(0.99 0.004 18)"
  light-copper: "oklch(0.5 0.12 52)"
  light-verdigris: "oklch(0.48 0.11 160)"
  light-focus: "oklch(0.55 0.11 85)"
typography:
  display:
    fontFamily: "Silkscreen, monospace"
    fontSize: "24px / 40px from 640px"
    fontWeight: 700
  pixel-sm:
    fontFamily: "Silkscreen, monospace"
    fontSize: "16px"
    fontWeight: 700
  head:
    fontFamily: "JetBrains Mono"
    fontSize: "20px"
  row:
    fontFamily: "JetBrains Mono"
    fontSize: "15px"
  body:
    fontFamily: "JetBrains Mono"
    fontSize: "14px"
  meta:
    fontFamily: "JetBrains Mono"
    fontSize: "12px"
    letterSpacing: "0.1em"
    textTransform: uppercase
rounded:
  none: "0px"
---

# Design System: weebo-notify

Source of truth: `web/src/styles/theme.css`. This file describes it; when they disagree,
the stylesheet wins and this file is wrong.

## Overview

**North star: "The Proof Sheet", second edition.** weebo-notify is BatleHub's sibling
(`batleforc/batlehub` DESIGN.md): the same Monofolio palette on two grounds, square
corners, hairline rules, no card and no shadow, Silkscreen for identity and JetBrains Mono
for every fact. What it owns is its signature: **a notification's lifecycle drawn as a
pixel matrix**.

- Still running (`open`, `working`) draws **coarse** — a 2×2 matrix; `working` chases.
- `closed` **resolves** to a fine 3×3 whose pattern names the status:
  success = all nine, failure = an X, warning = a plus, info = a column,
  cancelled = a diagonal.
- When a revision lands, its row plays the one authored motion, `resolve`
  (blur + contrast → crisp, 520ms). Never on first render, never on hover.

Pattern, word and hue always say the same thing (`StatusDot.vue`): never colour alone.

## Colors

Tokens are named for their job; components ask for `--signal`, never for a rendition.
`.dark` on `<html>` selects near-black; paper is the base declaration.

- **Signal (crimson)** — failure, urgent, the one filled action per view, the selected
  row's 1px edge, unread counts, links. Never on a disabled control.
- **Copper** — waiting: `working`, high priority, late delivery, pressed toggles.
  Never means "good".
- **Verdigris** — success, and only success. weebo-notify's one addition to BatleHub's
  palette: an alerting inbox is scanned for "green or not" before anything is read.
- **Ink / ink-dim** — every fact; ink-dim is the floor for small text (AA in both).
- **Rule soft / strong** — separators / interactive boundaries.
- **Focus (amber)** — the focus ring, nowhere else.

The shadcn vocabulary (`--primary`, `--muted-foreground`, …) is mapped onto these in
`theme.css` so vendored components keep working; new code uses the world tokens
(`text-ink-dim`, `border-rule`, `bg-signal`, …).

## Typography

- **Silkscreen** (self-hosted, OFL) only at multiples of 8px: 16 (section titles,
  `pixel`), 24 (inbox head, wordmark), 40 (page titles, `display`, ≥640px). Never fluid.
- **JetBrains Mono** for everything else: 20 detail title, 15 rows, 14 body, 12 `meta`
  (uppercase, tracked 0.06–0.1em). Notification bodies read at 1.7 line-height, 68ch.
- Numbers are `tabular-nums`.

## Layout

- Shell (§10.4): sidebar 17rem · list · detail 26rem (left/right/under is a preference).
- Below `lg` (1024px): top bar + **bottom tab bar** (Inbox, Channels drawer, Search,
  Settings), detail opens as a full-screen layer. Safe-area insets honoured.
- Pages (Settings, Admin, Console): `display` title, then sections separated by a
  full-width top hairline (`Card` is a section, not a box), max 3xl/4xl measure.
- Wide content scrolls in its own container; the body never scrolls sideways.
- Touch: `pointer-coarse` raises controls to 44px.

## Components

- **Buttons** — square, 1px, uppercase meta. `default` = crimson fill (one per view;
  hover ring, pixel-step on press); `outline` = the control; `ghost`; `destructive` =
  crimson outline. Disabled drops the fill.
- **Segmented group** (`.seg`) — every mutually exclusive choice (nav, filters, theme,
  locale, density): one frame, 1px dividers, chosen cell reversed to an ink block.
- **Bay item** (`.bay-item`) — channel list rows: selection is ink + crimson 1px edge +
  raised fill + weight.
- **Rows** (`.rule-row`) — dashed divider at rest, solid when hovered or selected.
- **Badges** — 1px square chips, uppercase meta.
- **Panels** — empty / error states: a 1px frame (crimson for errors), pixel heading.
- **Popovers, palette, toasts** — `ground-sunk` behind a 1px strong rule; no shadow.

## Do / Don't

- Do carry state on pattern + word + hue. Don't add a hue for a new condition.
- Do keep corners square and lines 1px (focus ring and tab indicator are the 2px exceptions).
- Don't add shadows, glows, gradients, or a coloured side stripe thicker than 1px.
- Don't animate anything but `resolve` and the `working` chase; both vanish under
  `prefers-reduced-motion`.
- Don't put texture (`.dot-field`) where text can land.
