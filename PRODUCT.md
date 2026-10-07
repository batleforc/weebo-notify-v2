# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

The same Vue bundle ships as a PWA and inside the Tauri desktop shell; mobile web
(phone, tablet) is a first-class target, not a degraded one.

## Users

- **The homelab / infra operator.** Runs the `weebo-si` stack and receives what it
  emits: CI pipelines, ArgoCD syncs, alerts, backups. Replaces Gotify and ntfy. Opens
  the inbox when something changed, often from a phone, often while something is
  broken.
- **A team of 5–50.** Shares channels, sees who has seen what (receipts), gets access
  through OIDC groups. Reads on desktop during the day, on phone on call.
- **The platform-admin.** Owns the GitOps config, trust policies, quotas, the sink
  DLQ and the audit log. Lives in the console after an incident.
- **Machines, as non-human users.** CI jobs, bots and services publish through API
  keys, service accounts or federated CI tokens. They never see the UI; a human uses
  the UI to understand what they sent.

## Product Purpose

A self-hosted, multi-channel, multi-device notification platform. Its core object is
a **notification as a lifecycle, not a message**: one identity, a state
(`open` → `working` → `closed`), a status and a history of revisions. A pipeline
pushes `open` when it starts and `closed/success` when it ends, under the same id —
one inbox entry that changed, not two lines of noise.

Success is a quiet inbox that is right: the reader knows at a glance what is still
running, what failed, and what needs them, on any device.

## Positioning

Gotify and ntfy deliver messages; weebo-notify tracks things that happen over time,
with revisions, actions (ack, reply, choice, silence), receipts, DND, offline sync
and federation with CI forges. Single binary, Postgres, OIDC, GitOps-declared
channels.

## Operating Context

- Web inbox (three zones: channels, list, detail), Cmd/Ctrl+K palette, permalinks
  `/n/<id>` and `/c/<id>` pasted into CI logs and emails.
- CLI and TUI (`notify`) share the vocabulary of the web actions.
- Web Push / PWA, desktop shell (Tauri), Android/iOS push transports server-side.
- Deployed by Helm or container next to Postgres and an S3-compatible store.

## Capabilities and Constraints

- Server: Rust (actix, utoipa/Scalar), Postgres, OIDC, API keys, service accounts,
  CI token exchange, sinks (webhook, SMTP), Prometheus metrics, OTLP traces.
- Web: Vue 3, Tailwind v4, shadcn-vue components vendored in `web/src/components/ui`,
  Reka UI primitives, HeyApi-generated client, i18n EN/FR.
- Strict CSP (no inline script), self-hosted fonts and assets only.
- Native Android (Kotlin) and iOS (Swift) clients are out of this repository's reach.

## Brand Commitments

- Sibling of BatleHub (`batleforc/batlehub`): same Monofolio palette line, square
  geometry, hairline rules, Silkscreen + JetBrains Mono; weebo-notify brings its own
  signature (the lifecycle state as a pixel matrix).
- Dark and light renditions both first-class; preference `system|light|dark`.

## Evidence on Hand

No real customer data, testimonials or metrics. Dev seeds (`dev/`) and test
identities (alice, bob, carol) are the only demo content.

## Product Principles

1. State before content: what is running, what failed, what needs me.
2. One notification, one row, however many revisions.
3. Every device is a real client; a phone is not a degraded desktop.
4. Never colour alone: state reads by pattern, word and hue.
5. Fits the weebo ecosystem: OIDC, GitOps, Helm, the same design language.

## Accessibility & Inclusion

WCAG 2.2 AA contrast in both renditions, full keyboard operation (parity with the
TUI), visible focus, `prefers-reduced-motion` honoured, EN/FR.
