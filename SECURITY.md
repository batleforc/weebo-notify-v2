# Security policy

## Reporting a vulnerability

**Report it privately on GitHub:** [open a security advisory](https://github.com/batleforc/weebo-notify-v2/security/advisories/new).
Or **email <maxleriche.60@gmail.com>** with `weebo-notify security` in the subject.

Please do **not** open a public issue for anything that looks exploitable. There is
no bug bounty and no legal safe-harbour programme behind this document — it is a
self-hosted project maintained by one person — but a report will be read, answered
and credited.

What helps, in rough order of usefulness:

- the version or commit, and whether it is the server, the web bundle, the CLI or the chart;
- what an attacker gains, concretely — "any authenticated user can read another channel's notifications" beats "missing authorisation check";
- the smallest reproduction you have, even an unpolished one;
- the deployment shape, if it matters: `attachments.kind`, whether the API and the web bundle share an origin, what sits in front of the ingress.

You will get an acknowledgement within **five working days**. If you have not heard
anything in ten, assume the mail was lost rather than ignored and send it again.

Coordinated disclosure is preferred, on whatever timeline the finding warrants —
something reachable by an unauthenticated caller is not the same as something that
needs a platform admin to be careless.

## What is in scope

The server (`crates/`), the web bundle (`web/`), the CLI, the Helm chart
(`charts/`), and the release pipeline (`.github/workflows/`, both Containerfiles).

Also in scope, and worth saying explicitly because it is where a real compromise
would most likely start: **the supply chain.** A way to get an unreviewed change
into a published image or a signed release is a vulnerability in this project even
though it is not a bug in its code.

## What is out of scope

Some guarantees are deliberately the deployment's rather than the application's, and
are documented as such in
[Hardening](https://github.com/batleforc/weebo-notify-v2/blob/main/docs/content/docs/ops/hardening.mdx):

- **encryption at rest** — the application performs none ([ADR-0007](docs/content/docs/adr)); it rests on encrypted disks and encrypted backups;
- **traffic inside the cluster** — application-plaintext by design, protected by network policy;
- **`frame-ancestors`, HSTS and per-address rate limiting** — these belong to whatever serves the bundle and whatever fronts the API, and the application cannot send or enforce them;
- **an object store left publicly readable** — the bucket must be private; a presigned URL is the only intended path to an object.

A report that one of these is missing in *your* deployment is a configuration
question, and very welcome as an issue. A report that the documentation is wrong
about one of them, or that the application undermines a guarantee it claims to
leave to you, is a security report — send that to the address above.

Also out of scope: findings against `dev/`, which is a disposable local stack whose
credentials are committed on purpose (design §12.1), and `crates/oidc-mock`, which
is a development identity provider that is never published and never deployed.

## Known accepted findings

Advisories this project has looked at and decided not to act on are recorded with
their reasoning and their revisit condition, rather than suppressed silently:

- [`.cargo/audit.toml`](.cargo/audit.toml) and [`deny.toml`](deny.toml) — Rust advisories;
- [`.trivyignore.yaml`](.trivyignore.yaml) and [`charts/weebo-notify/.trivyignore.yaml`](charts/weebo-notify/.trivyignore.yaml) — container and Kubernetes findings;
- [`.gitleaks.toml`](.gitleaks.toml) — test fixtures shaped like credentials.

If you think one of those arguments is wrong, that is a security report too — the
reachability claims are the load-bearing part, and they are the part most likely to
stop being true.

## Verifying a release

Images are signed with cosign keyless, by digest. Every release lists its digests
and the command that checks them:

```bash
cosign verify ghcr.io/batleforc/weebo-notify-v2@sha256:<digest> \
  --certificate-identity-regexp "^https://github.com/batleforc/weebo-notify-v2/\.github/workflows/release\.yaml@refs/tags/v" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

An image that does not verify did not come from this project's pipeline, whatever
it is tagged.

The Helm chart is published the same way, as an OCI artifact
(`oci://ghcr.io/batleforc/weebo-notify-v2/charts/weebo-notify`) signed by the same identity, and
each image carries a cosign-signed SPDX SBOM attestation per platform
(`cosign verify-attestation --type spdxjson …`, same identity flags).

## Before 1.0: open release gates

Nothing below can be closed in code, and a `1.0.0` tag is not cut while any of it is
open. They come from the [2026-08-10 state advisory](docs/content/docs/ops/state-advisory-2026-08-10.md):

- [ ] **SEC-P0-08** — rehearse the release pipeline on a throwaway pre-release tag
  (`v0.x.y-rc.0`), so the images, the SBOM extraction, the chart push and every
  signature run for the first time on a tag that does not matter.
- [ ] **SEC-P0-09** — read and act on the first real `task audit:image` output from
  that rehearsal (blocked on P0-08).
- [ ] **SEC-P2-39** — an **external penetration test**, commissioned from someone
  with no prior belief about how the system works, with
  [ADR-0023](docs/content/docs/adr) (the threat model) as the brief. Every review so
  far was code read by an assistant and tests it wrote; that is not a substitute.
- [ ] Findings from the pentest triaged: fixed, or accepted here with reasoning and
  a revisit condition like every other accepted finding.
