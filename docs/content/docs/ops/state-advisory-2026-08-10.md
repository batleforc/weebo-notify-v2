---
title: Security advisory — 2026-08-10
description: Pre-release security, dependency and availability review of weebo-notify at commit 0138581.
---

# State advisory — 2026-08-10

A pre-release review of weebo-notify: known vulnerabilities, dependency freshness,
container hardening, the application's own security surface, and what the deployment
needs before it can be called highly available.

**Subject:** commit `0138581`, branch `feat/m0-socle`, working tree clean.
**Verdict:** the security engineering is well above average for a 0.1.0 — the gaps
that matter are in *release mechanics* and *operational defaults*, not in the code.
Three items below are release blockers. None of them is a vulnerability.

## Method

Everything in this report was executed, not inferred. What was not executed is
labelled as such.

| Check | Tool | Result |
| --- | --- | --- |
| Rust advisories | `cargo-audit` 0.22.2, DB of 1198 advisories | 0 unsuppressed, 3 suppressed (§2.1) |
| Rust licence / source policy | `cargo-deny` 0.20.2 | `advisories ok, bans ok, licenses ok, sources ok` |
| JS/TS advisories | `pnpm audit --audit-level low`, `web/` + `docs/` | 0 |
| Repository + Containerfiles | `trivy fs --scanners vuln,misconfig,secret` | 0 HIGH/CRITICAL; findings at MEDIUM in §2.3, §5.4 |
| Helm chart | `trivy config` | 0 HIGH/CRITICAL; 3 MEDIUM + 2 LOW, all judged in §5.4 |
| Secrets, full history | `gitleaks detect` over 27 commits | 6 hits, all benign (§4.6) |
| Dependency freshness | `pnpm outdated` | 2 deprecated production packages (§2.4) |
| Code review | manual — auth, crypto, HTTP surface, config, adapters | §4 |
| **Container images** | **not run** | **no container engine in this workspace; see §3.1** |

Toolchain: rustc 1.97.1, pnpm 11.20.0, trivy 0.73.0.

## 1. Release readiness

| Area | State |
| --- | --- |
| Known CVEs | **Clean.** No unsuppressed advisory in any dependency tree that is gated. |
| Suppressions | **Sound.** Three, each with a written reachability argument and a revisit condition. |
| Application security | **Strong.** `unsafe_code = "forbid"`, Argon2id key hashing, alg-bound JWT validation, constant-time MAC comparison, no auth-oracle responses. The one exploitable defect this review found — a content-type allowlist bypass in the filesystem attachment backend (§4.4) — is fixed, as is the authentication-path denial of service (§4.1). |
| Container hardening | **Good on paper, unverified in practice.** Distroless, non-root, no shell — in a file that has never been built. |
| Supply chain | **Incomplete.** Cosign signs a mutable tag; actions and base images float; nothing tracks staleness. |
| Licensing | **Broken.** Four artefacts, three different licences (§6.1). |
| High availability | **Designed for it, with two operational traps** (§7). |

### Blockers for tagging v0.1.0

1. §4.4 — the filesystem attachment backend does not bind content-type to the upload token, so the allow-list can be bypassed and arbitrary HTML served from the API origin. A blocker for any deployment that would run `attachments.kind: filesystem`; the S3 path is unaffected.
2. §6.1 — licence metadata contradicts itself four ways.
3. §3.1 — the images have never been built, so no hardening claim about them is verified and the base-image CVE surface is unmeasured.
4. §6.2 — `cosign sign` targets a mutable tag rather than the pushed digest.

## 2. Dependencies

### 2.1 The three suppressed RustSec advisories

They are still live — verified by re-running `cargo audit` from a directory without
`.cargo/audit.toml`, which reports exactly these three and nothing else:

| Advisory | Crate | Fixed in | Assessment |
| --- | --- | --- | --- |
| RUSTSEC-2023-0071 (Marvin) | `rsa` 0.9.10 | *no patch exists* | **Argument holds.** The attack needs an RSA *private* key operation. `notify-oidc` only verifies signatures against JWKS public keys; `oidc-mock` signs ES256; minted federation tokens are Ed25519. There is no RSA private key anywhere in the product. |
| RUSTSEC-2026-0194 | `quick-xml` 0.38.4 | ≥ 0.41.0 | **Argument holds, with a caveat below.** |
| RUSTSEC-2026-0195 | `quick-xml` 0.38.4 | ≥ 0.41.0 | Same. Reached only through `rust-s3`, which pins `^0.38`. |

`.cargo/audit.toml` states each argument and each revisit condition, and `deny.toml`
mirrors the list. That is the right way to carry an ignore, and it is rare enough to
say so.

The caveat on `quick-xml`: the reachability argument is "this parser only ever reads
responses from our own object store, which is inside the trust boundary". Meanwhile
`hardening.mdx` states that pod-to-bucket traffic is application-plaintext by design.
The two are consistent — anyone positioned to inject XML on that path can already
read every attachment, so a denial of service is not the marginal risk — but the
argument depends on a deployment fact, not a code fact. Re-test it when `rust-s3`
moves, rather than treating it as settled.

### 2.2 `web/src-tauri` is outside every Rust gate

`web/src-tauri/Cargo.toml` declares an empty `[workspace]` on purpose (Tauri links
GTK/WebKit, which the development environment cannot install). The consequence is
that `task audit:cargo` — which runs `cargo audit` and `cargo deny` at the repository
root — never sees that dependency tree, and neither does the licence or source
policy.

`trivy fs` does read `web/src-tauri/Cargo.lock`, but `task audit:container` gates at
`HIGH,CRITICAL`. So the one finding currently there is invisible to the gate:

> `glib` 0.18.5 — GHSA-wrw7-89jp-8q8g (MEDIUM): unsoundness in the `Iterator` /
> `DoubleEndedIterator` impls for `glib::VariantStrIter`. Fixed in 0.20.0.

**Do:** add `cargo audit --file web/src-tauri/Cargo.lock` and a `cargo deny
--manifest-path web/src-tauri/Cargo.toml check` to `audit:cargo`, and bump the Tauri
dependency set. This matters more later than now — the desktop client is M5 — but the
gate should exist before the code it guards does.

### 2.3 Two production web dependencies are deprecated upstream

```
@hey-api/client-axios  0.9.1  → Deprecated
lucide-vue-next        1.0.0  → Deprecated
```

Both ship in the browser bundle. A deprecated package does not receive security
fixes, which makes "0 known vulnerabilities" a statement about today only. The
`@hey-api` one also sits directly on the generated API client — the piece of the
frontend most coupled to the OpenAPI contract, so migrating late is more expensive
than migrating now.

Everything else is within one or two patch releases, except TypeScript 6 → 7 in both
projects (a major, and not a security matter).

### 2.4 Nothing measures staleness

There is no Renovate or Dependabot configuration, and no `cargo outdated`. `task
audit` answers "is anything known-vulnerable today"; nothing answers "how far behind
are we", which is what determines how fast the project can *react* on the day the
first answer changes. A dependency five majors behind has no upgrade path under
incident pressure.

**Do:** add Renovate — one configuration covers Cargo, pnpm, Containerfile base
images and CI actions, which is exactly the four surfaces §6.3 also asks to pin.

## 3. Container hardening

### 3.1 Neither image has ever been built

Both Containerfiles say so in their own headers, and `release.yaml`'s build step is
explicit: *"THIS HAS NEVER RUN."* That honesty is worth more than a silent assumption,
but it has a consequence that should not be understated: **every hardening property
claimed for the runtime image is currently a claim about a file, not an observation
about an artefact.** Distroless, no shell, no package manager, `USER nonroot` — all
plausible, none executed. `task audit:image` has likewise never run, so the base
images' own package CVE surface is entirely unmeasured.

**Do, before tagging:** trigger the release pipeline on a throwaway pre-release tag
(`v0.1.0-rc.0`) — the workflow already treats a pre-release as a complete product with
the same artefacts, signatures and scans, which is precisely what makes it usable as a
rehearsal. Then read the `audit:image` output.

### 3.2 What the Containerfiles get right

Reviewed by reading; no findings.

- Multi-stage, with `gcr.io/distroless/cc-debian12:nonroot` as the runtime — no shell, no package manager, no root user to drop from.
- `cargo build --release --locked`, so a release cannot resolve a version that was never tested.
- rustls throughout (`sqlx`, `reqwest`, `rust-s3`, `lettre`), so there is no OpenSSL in the image and no OpenSSL CVE to inherit. Confirmed against `Cargo.lock`: the only `openssl`-named crate is `openssl-probe`.
- `USER nonroot:nonroot`, explicit `VOLUME` for configuration, no configuration baked in.
- The web image uses `nginx-unprivileged` and asserts `USER 101` rather than inheriting it.
- `.containerignore` excludes `dev/secrets/`, `.git/` and the local config overrides — the reasoning in that file (a build reads the working tree, not the index, so gitignored keys would otherwise land in a layer) is correct and is a mistake many projects make.
- No `HEALTHCHECK` in the distroless image, deliberately and correctly: there is no shell to run one, and Kubernetes probes `/healthz` and `/readyz` directly.

### 3.3 `EXPOSE 9090` is false

The root `Containerfile` exposes 9090 and describes it as *"the Prometheus exporter
… a separate listener so it can stay off the public ingress."*

There is one `HttpServer` (`crates/notify-server/src/main.rs:176`), and `/metrics`,
`/healthz` and `/readyz` are all registered on it
(`crates/notify-server/src/health.rs:99-101`). `/metrics` answers on the same :8080 as
the API. The Helm chart's README already documents this correctly and calls the
`EXPOSE` "aspirational"; the Containerfile does not.

This is a comment an operator would build a network policy on. Either delete the
`EXPOSE` and the sentence, or add the second listener.

## 4. Application security

This is the strongest part of the codebase. The findings below are refinements, not
defects — with one exception (§4.1) that is a real capacity and denial-of-service
concern.

### 4.1 Argon2id runs on every API-key request — no cache, no pre-auth rate limit

`Authenticate::api_key` (`crates/notify-app/src/auth.rs:250`) calls
`hasher.verify(...)` on every request that presents a `wnk_` key. The hasher is
`Argon2::default()` — 19 MiB, t=2, p=1 — on a `spawn_blocking` thread. There is no
verified-credential cache, and rate limiting lives *inside* the use cases
(`publish`, `read`, `federate`), which is to say **after** authentication.

The cheap paths are handled well: a malformed key is rejected on shape before any
I/O, and an unknown prefix costs one indexed read (`prefix TEXT NOT NULL UNIQUE`) and
no KDF. The exposure is the path where the prefix is *real*:

- **Capacity.** A CI fleet publishing through one key pays ~19 MiB and two Argon2 passes per request. That is a ceiling of roughly tens of requests per second per core, on the hottest path in the API — for a product whose headline use case is a CI pipeline pushing state transitions.
- **Amplification.** Anyone who learns an 8-character prefix (returned by `GET /keys` to its owner, printed at mint, and plausibly pasted into a log or a screenshot) can send garbage secrets at that prefix and force a full Argon2 evaluation per request, pre-rate-limit. Tokio's blocking pool defaults to 512 threads: ~9.7 GB of transient RSS and complete CPU saturation, from unauthenticated requests.

Three fixes, any of which resolves it:

1. **Cache the verdict.** Key on `(prefix, SHA-256(presented secret))` → `Caller`, 30–60 s TTL, bounded size. Simplest, preserves the KDF.
2. **Reconsider the KDF for this input.** Argon2 exists to make *low-entropy human passwords* expensive to guess. A `wnk_` secret is 160 bits of machine-generated randomness — there is no dictionary to run against it, and no offline attack that a memory-hard function makes meaningfully harder than HMAC-SHA-256 with a server-side pepper. This is the model GitHub and Stripe use for exactly this reason. It would also let the current `SecretHasher` port stay as-is for anything genuinely password-shaped.
3. **Cap `max_blocking_threads`** on the runtime regardless, so the worst case is queueing rather than memory exhaustion.

The docstring at `secrets.rs:14` argues the cost "is the point, so it is *not* tuned
down" — correct for the threat it names (a stolen database of hashes), and it does
not address the online cost. Both can be true; option 1 keeps the first while
removing the second.

**Related, same path:** `self.keys.touch(stored.key.id, now)`
(`auth.rs:278`) issues a Postgres write on every API-key request. It is
`tracing::warn`-on-failure rather than fatal, which is right, but it is still one row
update per authenticated call. Throttle it — write only when `last_used` is older
than ~60 s.

### 4.2 What the review found correct

Verified by reading the implementations:

**JWT validation** (`crates/notify-adapters/oidc/src/verifier.rs`) — algorithms are
bound to the algorithms declared by the matched JWK rather than to the token's own
`alg` header, which closes algorithm-confusion. Issuer is validated, audience is
validated on the user path, `exp`/`iss`/`aud` are all required spec claims, and
expiry is checked against the injected clock with a 60 s leeway so it is testable.
The federated path skips audience validation *at this layer* and carries it out to
`TrustPolicies::decide` — documented, and the right shape, since one place decides.

**API key hashing** — Argon2id with a per-key salt from `OsRng`, on `spawn_blocking`
(correct: a KDF on an async worker stalls every other request that worker holds). An
unparseable stored hash is reported as corruption rather than as a wrong secret,
which avoids telling a legitimate holder their good key is invalid.

**No authentication oracle** — unknown prefix, wrong secret and every token rejection
collapse to one `InvalidCredential`. Expiry and revocation *are* distinguished for
API keys, deliberately and with the reasoning written down.

**Constant-time comparison** — the filesystem attachment token
(`attachments-fs/src/token.rs:95`) folds an XOR accumulator over the rendered hex
rather than comparing strings. The webhook signature does the same.

**`unsafe_code = "forbid"`** at the workspace level; zero occurrences of `unsafe` in
`crates/`.

**Path traversal is structurally impossible** in the filesystem attachment store:
paths are `root.join(AttachmentId)`, and `AttachmentId` is a parsed ULID newtype — a
`../` never survives parsing.

**SSRF** — webhook URLs come from operator configuration, not user input, and
`config.rs:433-441` refuses any scheme but http/https and any URL carrying
credentials, at startup. Redirects are disabled on the client.

**Secrets never reach logs** — the sink startup log records counts and kinds, never
destinations, with the reason stated at the call site.

**CORS is closed by default** (`cors_origins: Vec::new()`), and there is no cookie or
session to protect in the first place.

**Attachment uploads** are bounded by `MAX_ATTACHMENT_BYTES` via `PayloadConfig`, and
avatar decoding checks dimensions before full decode, refuses animated formats and
strips metadata — the one place the server parses user-supplied media, analysed as
such in `hardening.mdx`.

### 4.3 API-key secret entropy is coupled to the ID generator

`UlidKeyGenerator::generate` (`crates/notify-app/src/keys.rs:341`) builds the secret
by concatenating two `ApiKeyId::generate()` ULIDs — the same constructor that mints
publicly-visible identifiers throughout the domain.

Today this is fine: `Ulid::new()` draws from a CSPRNG and two ULIDs carry 160 bits of
randomness, which is ample. It is fragile rather than wrong. ULID libraries commonly
offer a *monotonic* mode in which the random tail increments within a millisecond,
and `ApiKeyId` is a general-purpose ID type that nobody changing it would think of as
credential material. The failure mode — predictable API key secrets — is silent and
total.

**Do:** draw 32 bytes from `OsRng` and base64url-encode them. Three lines, and it
removes a coupling that only the comment currently prevents.

### 4.4 The filesystem backend does not bind content-type to the upload token

**This is the one exploitable defect in the review, and it is a divergence between a
documented invariant and the code.**

`FsAttachments::presign_upload` (`attachments-fs/src/lib.rs:216`) says:

> The client sends this header on the PUT; the upload route checks it matches, the
> way S3's signature checks it there instead.

The upload route does not check it. `fs_upload`
(`http-actix/src/routes/attachments_fs.rs:153-160`) reads `content-type` off the
request and hands it straight to `store.put`, and the HMAC token signs only
`(purpose, id, expires)` — the content type is not in the signed material and is
never compared to what the presign approved.

The S3 adapter *does* get this right: `presign_upload`
(`attachments-s3/src/lib.rs:154-165`) puts content-type into the `HeaderMap` passed
to `presign_put`, so it becomes a SigV4 signed header and a mismatched upload fails
the signature. The filesystem adapter was written to the same contract and only
implements half of it.

**The chain**, for any caller holding publish scope on any channel, when the
deployment runs `attachments.kind: filesystem`:

1. `POST /api/v1/attachments/presign` with `content_type: "image/png"` — passes the allow-list (`notify-app/src/attachments.rs:42`, which permits only png/jpeg/webp/gif).
2. Receive the presigned PUT URL and its token.
3. `PUT …/attachments/fs/{id}/upload?token=…` with `Content-Type: text/html` and an HTML body. Stored verbatim.
4. `GET …/attachments/fs/{id}/download?token=…` replies `Content-Type: text/html`, with no `X-Content-Type-Options: nosniff` and no `Content-Disposition`.
5. That download URL needs no credential — the token is the credential — so it can be handed to anyone.

**What it yields:** arbitrary attacker-controlled HTML and JavaScript served from the
API's own origin, over the organisation's TLS certificate, with the content-type
allow-list bypassed entirely. Because ADR-0021 keeps the web bundle on a separate
origin, this is not by itself SPA token theft — it is a high-credibility phishing
surface on a trusted internal hostname, and it becomes full XSS against the
application in any deployment that puts the bundle and the API on one origin.

The three byte-serving routes also omit the two headers that would have contained the
damage independently: `fs_download` (`attachments_fs.rs:199`), `thumbnail`
(`attachments.rs:127`) and `read_avatar` (`profile.rs:191`) all echo a stored
content-type with neither `nosniff` nor `Content-Disposition`. Avatars are the least
exposed of the three — the processor re-encodes to PNG and hardcodes the type
(`avatars/src/lib.rs:113`), so the value is server-controlled there.

**Do, in order:**

1. Put the content type into the signed material — `hmac(secret, purpose | id | expires | content_type)` — and have `fs_upload` refuse a PUT whose header disagrees. That is what the comment already promises.
2. Re-apply the `ALLOWED` allow-list at the upload route. Presign-time validation guards a different request than the one carrying the bytes; the payload ceiling is already re-checked at this boundary for exactly that reason, and the content type deserves the same treatment.
3. Send `X-Content-Type-Options: nosniff` and `Content-Disposition: attachment` on all three routes, unconditionally.
4. Consider verifying the magic bytes against the declared type. The allow-list is four image formats; a four-way sniff is cheap and closes "declared png, actually anything".

### 4.5 Unauthenticated surfaces, enumerated

For the record, since "which endpoints need no credential" is the first question an
auditor asks:

| Path | Guard |
| --- | --- |
| `/healthz`, `/readyz` | none — by design, kept off the Ingress |
| `/metrics` | none — by design, kept off the Ingress (see §5.2) |
| `/api/v1/attachments/fs/{id}/upload`, `.../download` | short-TTL HMAC token in the query string; the token *is* the credential, the same way a presigned S3 URL is |
| `/api/docs` | none — serves the OpenAPI document and Scalar UI |

Everything else takes the `Authenticated` extractor, and the extractor is the only
route from HTTP into the application layer.

Two notes. `/api/docs` publishes the full API surface unauthenticated; that is a
reasonable choice for a self-hosted product and worth being a deliberate one — add a
values toggle if any deployment would rather not. And the filesystem attachment
routes are only as private as `token_secret`: the chart comments already warn that an
empty one is "an unsigned, open directory", which is the correct level of alarm.

### 4.6 Gitleaks: 6 hits, all benign

Over the full 27-commit history, with the repository's own `.gitleaks.toml`:

| File | Rule | What it is |
| --- | --- | --- |
| `crates/notify-adapters/oidc/src/federation.rs:402` | jwt | a JWT literal inside a `#[cfg(test)]` assertion |
| `crates/notify-adapters/push-webpush/src/lib.rs:415` | private-key | a test VAPID key in the test module |
| `web/src/lib/push.spec.ts:30` | generic-api-key | a test fixture |
| `docs/content/docs/api/openapi.json`, `web/src/api/generated/types.gen.ts`, `crates/notify-ports/src/tokens.rs` | generic-api-key | the same prose fragment ("token itself, …") in a doc comment and its two generated copies |

No live credential. The problem is not these six — it is that a repository which
cannot produce a clean `gitleaks detect` teaches people to skim the output, and the
seventh hit is the one that matters.

**Do:** extend the `.gitleaks.toml` allowlist with these paths (or their
fingerprints), so a full-history scan is green and a new hit is a signal.

## 5. Kubernetes and the Helm chart

The chart is unusually complete for a 0.1.0: pod and container security contexts,
`readOnlyRootFilesystem`, `drop: ALL`, `RuntimeDefault` seccomp, `runAsNonRoot` with
the distroless uid asserted rather than assumed, every secret an `existingSecret`
reference with nothing sensitive in `values.yaml`, PDBs, HPAs, ServiceMonitor,
NetworkPolicies, and an Ingress scoped to `/api/v1` precisely so the health and
metrics endpoints stay off it.

Three things to fix.

### 5.1 The production example enables NetworkPolicy in a configuration that breaks the API

`examples/values-production.yaml` sets `networkPolicy.enabled: true` and leaves
`networkPolicy.ingressFrom` at its default `[]`.

The server's NetworkPolicy admits ingress only from pods labelled as the `web`
component, plus whatever `ingressFrom` adds. But the server has its *own* Ingress
(`/api/v1` → the server Service), and that traffic arrives from the ingress
controller's namespace — not from the `web` pod. The Prometheus scrape defined by the
ServiceMonitor arrives from somewhere else again.

So an operator who copies the production example verbatim — which is what a worked
example is for — gets a deployment whose API is unreachable from outside the
namespace and whose metrics are never scraped. `values.yaml` does document that
`ingressFrom` is "the one thing this cannot guess for you"; the example then does not
set it.

**Do:** put a real (commented, clearly-placeholder) `ingressFrom` selector in the
production example, covering both the ingress controller and the monitoring
namespace.

### 5.2 `/metrics` is protected only by path routing

Since there is no second listener (§3.3), the exporter is reachable at `:8080/metrics`
on the Service. Today the only thing keeping it private is that the Ingress declares
`path: /api/v1` with `pathType: Prefix`. That holds for the shipped chart, and stops
holding the moment an operator fronts the Service with a catch-all path, a second
Ingress, a `type: LoadBalancer` Service, or a service mesh gateway.

**Do:** state it in `hardening.mdx` next to the existing "two security headers"
Callout — it belongs to the same class of "the deployment owes this, because the
application deliberately does not".

### 5.3 The filesystem attachment backend is not an HA configuration

Covered in §7.2 — noted here because it is a chart behaviour: choosing
`attachments.kind: filesystem` silently forces `replicaCount: 1`, disables the PDB
and switches the rollout to `Recreate`.

### 5.4 Trivy findings, judged

`task audit` gates at HIGH,CRITICAL and is green. At MEDIUM and below:

| Finding | Judgement |
| --- | --- |
| KSV-01010 — ConfigMap stores `access_key` / `secret_key` | **False positive.** Both render as `""`; the real values arrive as `WNK_`-prefixed environment from an `existingSecret`. The template comment says so. |
| KSV-0125 (×3) — image from an untrusted registry | **False positive.** `ghcr.io` is the project's own registry. Configure trivy's trusted-registry list. |
| KSV-0011 (×2, LOW) — no `resources.limits.cpu` | **Correct as-is, deliberately.** A CPU limit causes throttling under burst; a memory limit is what prevents a noisy neighbour, and `server.resources.limits` sets one. |
| GHSA-wrw7-89jp-8q8g — `glib` 0.18.5 in `web/src-tauri` | **Real, low impact, invisible to the gate.** See §2.2. |
| jwt-token in `federation.rs:402` | **False positive** — test fixture. See §4.6. |

**Do:** write the three template findings into `charts/weebo-notify/.trivyignore`
with their reasons, the way `KSV-0014` already is. The gate currently sits at
HIGH,CRITICAL, which means it cannot distinguish "the MEDIUMs are understood" from
"nobody has looked" — and that distinction is the whole value of an ignore file.

## 6. Supply chain and release

### 6.1 Licence metadata contradicts itself four ways — blocker

| Artefact | Declares |
| --- | --- |
| `LICENSE.md` | **Apache-2.0** |
| `Cargo.toml` (`workspace.package.license`) | **MPL-2.0** |
| `Containerfile`, `web/Containerfile` (OCI label) | **MIT** |
| `charts/weebo-notify/Chart.yaml` (`artifacthub.io/license`) | **MIT** |

Three different licences across four places, none of them agreeing with the file that
actually ships. A consumer cannot determine what they are permitted to do, and
`deny.toml`'s allow-list is justified in its own comments against MPL-2.0 — a licence
the repository does not carry.

This is not a vulnerability and it is not hard to fix, but it is the kind of thing
that is far cheaper to correct before a first public tag than after. Pick one and
propagate it to all four.

### 6.2 Cosign signs a mutable tag — blocker

```yaml
cosign sign --yes "${{ env.IMAGE_NAME }}:${{ steps.kind.outputs.tag }}"
```

The tag is resolved at signing time, not at push time. Two consequences: the window
between the push step and the sign step is a window in which the signature can attach
to something other than what was built, and — more practically — consumers who verify
by digest (which is what an admission controller does) will not necessarily find a
signature for the digest they are running.

**Do:** take `steps.<id>.outputs.digest` from `docker/build-push-action` and sign
`${IMAGE_NAME}@${digest}`. Keyless identity and Rekor transparency are already right;
this is the last link.

The rest of the release pipeline is well built: gates replayed on tag, `provenance:
mode=max`, `sbom: true`, images scanned *after* push (correctly reasoned — what is
scanned must be what was published), tags never move, jobs idempotent.

Note that the final "Publish the release" step is still a stub that echoes a warning
— deliberate, deferred to M5 with the artefact matrix. Worth confirming that shipping
0.1.0 with no GitHub Release object, no checksums and no attached SBOM is the
intended shape of the first release.

### 6.3 Everything floats on mutable references

- **CI actions:** `actions/checkout@v4`, `docker/build-push-action@v6`, `docker/login-action@v3`, `docker/setup-buildx-action@v3`, `docker/setup-qemu-action@v3`, `sigstore/cosign-installer@v3` — all major-version tags, all mutable.
- **Toolchain bootstrap:** every job runs `curl -fsSL https://mise.run | sh`, an unpinned remote script, in a container that also holds `secrets.REGISTRY_TOKEN`. `mise.lock` pins the tools it then installs, which is good — but not the installer that fetches them.
- **Base images:** `rust:1.97-slim-bookworm`, `gcr.io/distroless/cc-debian12:nonroot`, `node:24-slim`, `nginxinc/nginx-unprivileged:1.29-alpine` — all tags.

The root `Containerfile` states the goal itself: *"a release image is the one artefact
that should not change because a base tag moved between two runs of the same
pipeline."* A tag does not achieve that. `distroless:nonroot` in particular is
rebuilt frequently.

**Do:** pin actions to commit SHAs, pin base images to digests, and let Renovate
(§2.4) keep both current. That converts three floating surfaces into reviewable diffs.

### 6.4 Minor: the CI toolchain installs more than CI needs

`mise.toml` lists `claude`, `rtk` and `herdr` alongside the build tools, and CI runs
`mise install --yes`, so the runners fetch all of them. They are pinned by
`mise.lock`, so this is hygiene rather than exposure — but every tool installed in a
job holding a registry token is attack surface that job does not use.

**Do:** split the developer-only tools behind a mise profile, or have CI install a
named subset.

## 7. High availability

### 7.1 What is already right

The design does the hard part properly, and `high-availability.mdx` is accurate:

- **Stateless API.** No server-side sessions — authentication is JWKS validation and key verification, so any replica serves any request. No sticky sessions, no session replication.
- **Live streams survive failover.** Each replica holds its own SSE/WebSocket connections; the event bus carries only pokes, and the global cursor means a client reconnecting to a different replica with `Last-Event-ID` gets no gaps and no duplicates. A lost poke costs latency, never correctness.
- **Background jobs are leader-elected** — Postgres advisory lock by default, Kubernetes Lease optionally. Non-leaders keep serving.
- **Expand/contract migrations** with `/readyz` gating entry, and a `SIGTERM` drain (`shutdown_timeout(15)` against `terminationGracePeriodSeconds: 30` — consistent).
- **The identity provider is not a SPOF.** Issued tokens stay valid on cached JWKS; API keys and CI tokens are signed locally. An Authentik outage stops new logins, not work.
- **Chart defaults** are `replicaCount: 2`, PDB `minAvailable: 1`, soft anti-affinity, HPA available.

Clean shutdown is handled with unusual care — jobs aborted before the pool closes, so
a sqlx connection is never dropped after the runtime dies, and the trace exporter
flushed last so the shutdown line is in the batch.

### 7.2 Trap 1 — the filesystem attachment backend silently collapses HA

Selecting `attachments.kind: filesystem` makes the chart force `replicaCount: 1`,
disable the PDB and switch the Deployment strategy to `Recreate`. This is correct
behaviour given a ReadWriteOnce PVC — two writers would corrupt it — and ADR-0022
records the debt.

The risk is that it is a *quiet* downgrade of an availability property, chosen by a
storage field. A rolling update becomes a full outage; a node drain becomes a full
outage.

**Do:** surface it in `NOTES.txt` at install time, not only in the chart README, and
recommend `kind: s3` for any deployment that has an availability target at all.

### 7.3 Trap 2 — Postgres carries four roles, and connection sizing is undocumented

Postgres is the database, the event bus (LISTEN/NOTIFY), the rate limiter, and the
leader lock. The docs name it as a SPOF and defer HA to an operator (CloudNativePG,
Patroni), which is the right scope call. What is missing is the sizing consequence:

- `[database] max_connections` defaults to **16 per replica**.
- Each replica additionally holds a dedicated LISTEN connection, plus the background job loops on the leader.
- The HPA's default ceiling is **6 replicas**. At full stretch that is ~100 connections before anything else on the cluster connects.

And the trap inside the trap: the obvious remedy — putting PgBouncer in front — **breaks
LISTEN/NOTIFY in transaction pooling mode**. The event bus connection must bypass the
pooler or use session mode. Neither `configuration.mdx` nor
`high-availability.mdx` mentions pooling at all, so the first operator to hit
connection exhaustion will reach for the tool that silently breaks live streams.

**Do:** add a sizing paragraph to `high-availability.mdx` — connections per replica ×
`maxReplicas`, the required Postgres `max_connections`, and an explicit warning about
transaction-mode pooling versus the event bus. This is the cheapest item in this
report and probably the one most likely to prevent a real incident.

### 7.4 Smaller HA gaps

**Spreading is best-effort by default.** `podAntiAffinity: soft` and
`topologySpreadConstraints: []` mean two replicas can land on one node, so
`minAvailable: 1` protects against pod eviction but not node loss. Recommend a hard
`topologySpreadConstraints` entry on `kubernetes.io/hostname` in the production
example.

**The HPA scales on CPU, and the saturating resource is connections.** The chart
documents the custom-metrics path (`sse_connections` via Prometheus Adapter or KEDA)
and leaves a `customMetrics` hook — good. Until one exists, CPU is a weak proxy for a
workload dominated by long-lived idle streams.

**There is no HPA `behavior` block.** Every scale-down event terminates a replica and
disconnects all of its live SSE clients, who then reconnect and replay from their
cursor. Without a `scaleDown` stabilization window, a workload that oscillates around
the CPU target will churn streams continuously. Add a conservative
`behavior.scaleDown.stabilizationWindowSeconds` (300 s or more) as a default.

**Reconnect herds on rollout.** Every rolling update disconnects every stream on the
drained replica at once, and they all reconnect with `Last-Event-ID` and replay. The
cursor makes this correct; it does not make it cheap. Worth measuring at the intended
connection count, and worth adding jitter to the client reconnect backoff if it is
not there already.

## 8. Prioritised actions

The triage view. **The working list is §12**, which carries every item in this report
as a checkbox with the file to touch — this section exists so the shape of the work
is visible without reading sixty of them. The two are deliberately not duplicated:
if they disagree, §12 is the one that is maintained.

**P0 — before tagging v0.1.0.** One exploitable defect (§4.4), and three release
mechanics that are cheap now and expensive after a public tag: licence metadata
(§6.1), images that have never been built (§3.1), and signing a mutable tag (§6.2).

**P1 — before, or immediately after.** The denial-of-service on the authentication
path (§4.1), a production chart example that does not work (§5.1), the Postgres
sizing trap (§7.3), a dependency tree outside every gate (§2.2), a false claim in the
Containerfile (§3.3), and two deprecated frontend packages (§2.3).

**P2 — hardening.** Supply-chain pinning, credential entropy hygiene, scanner and
secret-scanner allowlists, chart defaults, and the process gaps — `SECURITY.md`, a
threat model, fuzz targets, a rotation runbook.

**Beyond this release.** Envelope encryption at rest (§11.5, Tier 1) is the largest
security gain per unit of effort in this report, and it still belongs after
everything above.

## 9. Reproducing this report

```bash
task audit              # cargo audit + cargo deny + pnpm audit + trivy fs + trivy config
task audit:image        # needs built images and a container engine — not run here
gitleaks detect -c .gitleaks.toml --redact
pnpm -C web outdated && pnpm -C docs outdated
```

To see the three suppressed advisories rather than the suppression, run `cargo audit`
from a directory that contains only a copy of `Cargo.lock` — `.cargo/audit.toml` is
resolved relative to the working directory, so running it in-tree will always report
zero.

## 10. An adversarial reading

<Callout type="warn">
  **Authorship and standing of this section.** Everything below is written entirely
  by Claude (Anthropic's Claude Opus 5), acting as a coding assistant inside this
  repository at the maintainer's request. It is an engineering opinion, not a
  professional security audit: no human security engineer has reviewed it, no
  exploit was executed against a running instance, and nothing here has been
  validated by a certified assessor. **It carries no warranty and creates no
  commitment, endorsement or liability on the part of Anthropic**, which is not a
  party to this review and has not assessed this software. Sections 1–9 report what
  tools actually measured; this section is deliberately more speculative, and should
  be read as a list of hypotheses worth testing rather than as findings. It does not
  replace a penetration test by people, and a product handling notification content
  for an organisation should have one before it is trusted with anything sensitive.
</Callout>

The rest of this report answers "what is wrong". This section answers a different
question: **if I wanted in, where would I actually go?** The ranking is by what I
would try first, which is not the same as by severity — it is by the ratio of payoff
to effort, which is how an attacker allocates their time.

The short version: the application code is a hard target and I would not spend long
on it. The soft parts are the release pipeline, the operator's configuration, and one
adapter that promises a check it does not perform.

### 10.1 Attack paths, in the order I would try them

#### A1 — Bypass the attachment allow-list and host my own HTML on your API origin

*Verified, exploitable today, filesystem backend only. Full detail in §4.4.*

I present an approved `image/png` at presign time, then PUT `text/html`. Nothing
compares the two, because the token signs `(purpose, id, expires)` and stops there.
The download URL needs no credential, so I can send it anywhere.

I would go here first because it is the only place I found where a comment asserts a
control that the code does not implement — and that gap is worth more than a missing
control, because everyone downstream has already reasoned as though the check exists.

**Prevent it by:**

- Signing the content type into the upload token and refusing a PUT that disagrees.
- Re-applying the `ALLOWED` allow-list at the route that carries the bytes, not only at the route that approves them.
- Sending `X-Content-Type-Options: nosniff` and `Content-Disposition: attachment` on every route that returns stored bytes.
- Sniffing the magic bytes: four image formats is a four-way check.
- Keeping the API and the web bundle on separate origins, which ADR-0021 already does — it is what downgrades this from XSS to phishing, and it is worth stating as a security property rather than a deployment detail.

#### A2 — Take the instance down with a string anyone can read off a screenshot

*Verified reasoning, not executed. See §4.1.*

I do not need a valid key. I need eight characters — the prefix half of
`wnk_<prefix>_<secret>`. That is not a secret: it is printed at mint, returned by
`GET /keys`, and it is the part people leave visible when they redact a key in a
ticket, a screenshot or a CI log, because the tail is "the secret part". Once I have
one, every request I send with a garbage tail costs the server a full Argon2id
evaluation — 19 MiB and two passes — on a blocking thread, before any rate limiter
runs. A few hundred concurrent connections is enough to exhaust the blocking pool and
push the pod past its memory limit; the OOM kill takes every stream on that replica
with it, and every replica is equally reachable.

This is the highest-payoff attack in the product: unauthenticated, trivially
scripted, and it produces a total outage rather than a slowdown.

**Prevent it by:**

- Caching the verification verdict for `(prefix, SHA-256(secret))` with a short TTL, so a repeated wrong guess costs a hash-map lookup rather than a KDF.
- Keeping a small negative cache too — the attack is the *same wrong secret* thousands of times, which is the easiest possible thing to memoise.
- Reconsidering Argon2 for a 160-bit machine-generated secret: there is no dictionary to defend against, so HMAC-SHA-256 with a server-side pepper gives the same offline resistance at a fraction of the online cost.
- Rate limiting *before* authentication, keyed on source address and on prefix, in middleware rather than in the use cases.
- Capping `max_blocking_threads` so the failure mode is queueing, not memory exhaustion.
- Adding an ingress-level connection and request-rate limit as defence in depth — this is the one class of attack the application cannot fully solve alone.
- Treating the prefix as public in the documentation, so nobody builds a control on the assumption that it is not.

#### A3 — Skip the application and attack the pipeline that builds it

*Assessment, not a finding. Underlying facts in §6.2 and §6.3.*

Honestly: this is where I would spend my time if A1 and A2 were fixed. The
application is written by someone who thinks about failure modes; the pipeline is
not held to the same standard, and it produces the artefact everybody trusts.

The `publish` job holds `secrets.REGISTRY_TOKEN` and `id-token: write` — the ability
to push images and to obtain a keyless signing identity. Into that job it curls an
unpinned remote script (`https://mise.run`) and runs six actions pinned only to
mutable major tags. Compromise any one of those — the domain, the script, a tag on
any of six third-party repositories — and I do not need a vulnerability in
weebo-notify at all: I ship a malicious image that carries a valid cosign signature
and full SLSA provenance, and every verification an operator performs passes. The
tag-based signing in §6.2 widens the same window from the other end.

**Prevent it by:**

- Pinning every action to a full commit SHA, and letting Renovate propose the bumps as reviewable diffs.
- Pinning base images by digest, which the root `Containerfile` already argues for in prose.
- Vendoring or checksum-verifying the mise installer rather than piping a live URL into a shell in a privileged job.
- Signing the digest emitted by the build step, never a tag.
- Splitting the job that holds the registry token from the job that builds, so the build stage never sees a credential.
- Scoping `REGISTRY_TOKEN` to a single package and rotating it on a schedule, and preferring the runner's own ephemeral token where the forge offers one.
- Enforcing the signature at admission (Kyverno, Sigstore policy-controller) — an unverified signature protects nobody, and right now nothing in the chart requires one.

#### A4 — Wait for an operator to leave a door open

*Assessment. The individual facts are in §5.1, §5.2, and the existing `hardening.mdx`.*

weebo-notify deliberately pushes several guarantees onto the deployment — encryption
at rest, `frame-ancestors`, HSTS, bucket privacy, network policy. That is a
defensible architecture and it is documented honestly, which is more than most
projects manage. But it means the realistic breach is not a bug in this code: it is
a cluster where one of those was never applied, and **every one of them fails
silently.** Nothing logs "your bucket is world-readable". Nothing warns "you are
serving this app without `frame-ancestors` and it can be framed". The chart's own
production example ships a NetworkPolicy configuration that breaks the API (§5.1),
which is the same class of problem pointed inward.

I would scan for weebo-notify instances, look for a `/metrics` reachable through a
catch-all ingress, an attachment bucket that answers an anonymous GET, and a missing
`X-Frame-Options` — none of which requires touching the application at all.

**Prevent it by:**

- Adding a startup self-check that logs a prominent warning for each deployment-owned guarantee it can observe: no TLS ahead of it, an attachment bucket that answers unauthenticated, a `token_secret` that looks like a placeholder, `cors_origins` set to `*`.
- Shipping a `notify-server doctor` subcommand an operator can run against a live deployment and paste into an issue.
- Turning `hardening.mdx` into a checklist with checkboxes rather than prose, and printing a link to it in `NOTES.txt` at install time.
- Making the chart's production example correct end to end, and testing it in CI with `helm template` plus a policy check, so the file people copy is the file that works.
- Defaulting `networkPolicy.enabled` to `true` once `ingressFrom` has a sane shape — a security default that must be opted into is a security default most people will not have.

#### A5 — Contribute a translation

*Assessment. No current defect; the code is correct today.*

Three components render `v-html` over i18n output (`PushPanel.vue:69`,
`DndPanel.vue:253/268/301`). Every interpolation today is a literal, and each site
carries a comment saying "static, locale-file text only — never anything a caller
supplies". That is the right control and it is being applied consistently.

It is also a control that lives in a comment. The moment this project accepts
community translations — which a self-hosted notification tool eventually will — a
locale file becomes executable content, and a pull request adding Portuguese is a
pull request adding JavaScript. Reviewers do not read locale diffs the way they read
code diffs; that is precisely why I would use one.

**Prevent it by:**

- Running locale strings through a sanitiser at render time, so the safety is enforced rather than reviewed.
- Better: replacing `v-html` with Vue slots or `<i18n-t>` component interpolation, which lets a translator place a `<strong>` without ever handling markup.
- If `v-html` stays, restricting those strings to a tiny tag allow-list and adding a unit test that asserts every locale value is free of anything else.
- Treating `web/src/locales/**` as a code-owned path requiring security review.

#### A6 — Own the developers through the CLI the server hands out

*Assessment, worth verifying. Route at `http-actix/src/routes/cli.rs`.*

`GET /api/v1/cli` serves an executable to anyone merely authenticated — no scope
narrowing, deliberately. The binary comes from the running image, which is the right
design, and it is handed over with no checksum, no signature and no way for the
recipient to verify it is the one the project built. The people downloading it run it
on their workstations, which are the machines holding the credentials that matter.

So the question is not whether the endpoint is exploitable — it is what an attacker
gets from any of the *other* paths in this list. A compromised image (A3) or write
access to `[cli] binary_path` converts a server-side foothold into code execution on
every developer and CI runner that fetches the CLI. That is a much larger prize than
the notification database, and it is a step nobody has to be tricked into taking.

**Prevent it by:**

- Serving a detached signature and a digest alongside the binary, and having `notify` self-update verify them before replacing itself. The minisign custody described in `hardening.mdx` for the desktop updater is the same problem — solve both with one key.
- Publishing the expected digest in the release notes so it can be checked out of band.
- Documenting that `[cli] binary_path` is a code-execution surface for every client and must be inside the image, never on a writable mount.
- Considering whether "any authenticated principal" is the right audience, or whether a machine identity publishing to one channel should be able to pull an executable at all.

#### A7 — Exhaust the stream slots

*Hypothesis; I did not verify the accounting, so treat this as a test to run rather
than a claim.*

Each SSE and WebSocket connection holds a §8.2 slot and buffers up to
`BACKPRESSURE: 64` events. The slot bookkeeping is careful — one guard for the slot
and the gauge, precisely because the decrement is the hard part — and the code's own
comment notes that a leaked slot "locks somebody out of their own inbox until the
replica restarts". Slots appear to be per-principal, which makes the direct attack
self-inflicted. What I would test is whether the *aggregate* is bounded: many
principals, or many connections opened and abandoned at the TCP level so the
departure is only noticed 50 seconds later (the measured SSE figure in the same
comment), against a replica with a fixed memory limit.

**Prevent it by:**

- Load-testing connection churn explicitly — open, abandon at the socket level, repeat — and watching RSS and the gauge rather than the happy path.
- Adding a global connection ceiling per replica alongside the per-principal one, so the HPA scales instead of the pod dying.
- Limiting concurrent connections per source address at the ingress, where the address is actually known.
- Confirming the gauge that feeds the HPA cannot be driven by an attacker into scaling the deployment up as a cost attack.

### 10.2 What I would want secured that this report has not otherwise covered

These are not defects. They are the things whose absence I noticed, in roughly the
order I would add them.

- **A published security contact.** There is no `SECURITY.md`, no disclosure policy and no advertised address. Today a researcher who finds what §4.4 describes has no channel except a public issue. This is the cheapest item in the entire report and it is the one that determines how you find out about the next §4.4.
- **A written threat model.** The ADRs and `hardening.mdx` carry a great deal of implicit threat reasoning — they are unusually good at saying *why* — but nowhere states who the adversary is. Is it an internet-facing instance, or one behind a VPN? Is a publisher with an API key trusted not to attack readers? A2 and A5 are both interesting only under a particular answer, and the answer is currently in people's heads.
- **Fuzzing for the untrusted-input parsers.** There are no fuzz targets. The obvious candidates all parse attacker-controlled strings and all have hand-rolled logic: `Cursor`, `PresentedApiKey::parse`, `Pattern::matches`, the token parser in `attachments-fs`, and the markdown pipeline. `cargo-fuzz` over those five, run nightly, is a day of work and would cover the class of bug that code review is worst at.
- **A key rotation runbook.** Rotation is documented for release signing keys. It is not documented for the ones an operator actually holds: `token_secret`, the VAPID key, the federation signing key, the database credential. Each has a different blast radius and at least one — rotating `token_secret` — invalidates every presigned URL in flight, which somebody should discover from a document rather than from an incident.
- **Tamper-evidence on the audit log.** There is an audit log and it records the right things. Worth deciding whether a platform admin can delete from it, whether TTL purge can reach it, and whether that is the intended answer.
- **Authorisation tests written adversarially.** The test suite is genuinely good, and it is written to confirm intended behaviour. What I did not find is the mirror image: a test that asserts a key scoped to channel A cannot read channel B, one per boundary, written as an attack rather than as a scenario. Those are the tests that fail when a refactor widens a grant.
- **A pentest before anything sensitive lands in it.** Stated plainly because the disclaimer above means it: an assistant reading code finds a different class of bug than a person with a proxy and a week. The two are complements, and this review is only the first one.

### 10.3 What I could not assess

For completeness, since a review's blind spots matter as much as its findings:

- **Nothing was executed against a running instance.** No request was sent, no exploit run. A1 is derived from reading three files and checking that the S3 adapter does what the filesystem one does not; it is not a demonstrated exploit.
- **The container images do not exist** (§3.1), so the runtime attack surface — base image packages, actual filesystem permissions, whether `readOnlyRootFilesystem` holds — is entirely unexamined.
- **No Kubernetes cluster was involved.** The chart was rendered and scanned, never installed.
- **Frontend review was limited** to the XSS-relevant paths — markdown rendering, `v-html`, the CSP construction. Application logic, token storage in the SPA, and the OIDC redirect flow were not reviewed, and the last of those is where browser-side authentication usually goes wrong.
- **The mobile and desktop clients (M5–M7) do not exist yet**, so the largest future attack surface — a signed auto-updater and a push credential on a device — is out of scope by construction.

## 11. Confidential notifications — what a server-blind content class would take

<Callout type="warn">
  Same standing as §10: this section is written entirely by Claude and is design
  analysis, not an audit finding. It describes work that does not exist yet, and it
  creates no commitment on Anthropic's part. Cryptographic protocol design in
  particular is a field where confident-sounding prose is cheap and mistakes are
  expensive — treat the roadmap below as a starting point for an ADR, not as a
  specification to implement.
</Callout>

### 11.1 What the product encrypts today, and what that protects against

The existing model is stated precisely in `notify-domain/src/device.rs` and is worth
restating because it is easy to read as more than it is:

| Transport | Third party carries it | Protection | Device key |
| --- | --- | --- | --- |
| SSE / WebSocket / sync | no | TLS to us | none |
| Web Push | the browser's push service | RFC 8291, keys held by the browser | none |
| UnifiedPush | the distributor | sealed box | X25519 |
| APNs | Apple | sealed box | X25519 |

**This is relay protection, not end-to-end encryption.** It answers "can Apple read
my notifications" with a firm no. It does not answer "can the weebo-notify server
read my notifications", and the answer to that one is yes — by design, everywhere, at
every point in the lifecycle. `notifications.title` and `notifications.body` are
plaintext columns; ADR-0007 declines application-level encryption at rest entirely
and hands confidentiality to disk and backup encryption.

That is a coherent position and the documentation is honest about it. It is also the
thing that has to change if a notification's *content* is to be secured against the
platform itself — which is what a channel carrying incident details, customer data,
credentials in an alert body, or anything under a regulatory regime would actually
need.

### 11.2 The assets you already have

Unusually many, which is why this is a feasible feature rather than a rewrite:

- **A correct sealed box.** `notify-sealed-box` is ephemeral X25519 → HKDF-SHA256 → XChaCha20-Poly1305, with the ECDH point run through a KDF rather than used raw, a random 192-bit nonce rather than a derived one, and — the detail that tells you it was written by someone paying attention — a `was_contributory()` check that refuses low-order recipient keys. The HKDF info string is already versioned (`weebo-notify sealed-box v1`), so a second construction can coexist with the first.
- **Hardware-held device keypairs**, with the private half in Android Keystore or the iOS Keychain, and the invariant enforced from the first release so no device exists that would need re-enrolling.
- **A poke delivery path that is already normal operation.** `PushBody::Poke` sends identifiers only and lets the client fetch over TLS, because rich notifications already overflow transport size caps. A confidential notification would simply always take that path — the mechanism exists and is tested.
- **The lifecycle/content split, which is the real gift.** weebo-notify's differentiator is that a notification is a state machine, not a message. `state`, `status`, `priority_rank`, `cursor_seq`, `external_id` and the revision history are all *structural* — none of them is content. Sealing `title`, `body`, `tags` and action labels while leaving the lifecycle in the clear preserves the inbox, the state transitions, the cursor, deduplication by `external_id`, TTL purge, fanout routing and the whole ordering guarantee. Most products attempting this have to give up their core feature; you would not.
- **Digests already survive it.** `notify-ports/src/preferences.rs` is explicit that the held-back tally is *a counter*, not a row per notification — "the digest says 12 notifications". A digest names no notification and belongs to no channel. It needs no plaintext and would work unchanged.
- **Fanout routes on channel, priority and tags**, not on body text.

### 11.3 What breaks, specifically

Each of these reads content server-side today. A confidential class has to either
lose the feature, move it to the client, or replace it with something weaker.

| Feature | Where | Why it breaks | Realistic answer |
| --- | --- | --- | --- |
| Full-text search | `notifications.search TSVECTOR`, GIN index (`20260803000000_initial.sql:182`) | A server-side inverted index over `title`/`body`/`tags` is the definition of the server reading content | Client-side search over the local cache; or blind indexing on tags only. Encrypted-search schemes leak more than their papers suggest — I would not build one. |
| Email sink | `sinks-email` renders subject `[channel] title` plus the body | Mail is plaintext at the relay and at rest in the mailbox; the server must read content to compose it | Not available for confidential channels. Send a "you have a notification" stub with a link, or refuse the pairing at config-validation time. |
| Webhook sink | `sinks-webhook` POSTs the whole revision event | Same — the consumer expects readable JSON | Deliver the sealed blob and publish the unsealing contract, or refuse the pairing. Refusing is honest and much simpler. |
| Web Push payload | `PushBody::Payload { sealed: false }` | RFC 8291 encrypts to keys *the browser holds internally* — there is no application key on the web, so the server must supply plaintext | Force `Poke` for confidential notifications on web. This is the single largest client-side change, because it also means the browser needs a keypair (§11.4). |
| Attachment thumbnails | `notify-avatars`, called from the attachment path | The server decodes the image to resize it | Client-side thumbnailing, or no thumbnails for confidential attachments. Attachments themselves need envelope encryption with the key sealed alongside the notification. |
| Action labels | `action.rs:80`, `action.rs:228` | Interactive buttons carry user-visible text | Seal them with the rest of the payload; the *action id* the client posts back stays in the clear. |
| `group_key`, `tags` | plaintext columns, both indexed | Used for grouping and filtering, both server-side | Decide per field. Tags are frequently more sensitive than the body — a tag reading `customer-acme` leaks the thing the body was protecting. |

### 11.4 The problems that are not code

These are what actually determine whether the feature is a quarter of work or a year.

- **A channel is a group, and groups are the hard part.** Sealing to one device is solved (`notify-sealed-box`). Sealing to *every current member's every device*, and re-sealing when somebody joins or leaves, is group key management. The honest options are a sender-key scheme (simple, no forward secrecy, re-key on every membership change) or MLS via `openmls` (correct, forward-secret, post-compromise secure, and a substantial dependency and protocol commitment). Do not invent a third.
- **Your publishers are machines that hold nothing.** A CI job authenticates with an API key or a short-lived federated token and vanishes. For the publisher to encrypt, it needs the channel's current member device-key set at publish time and the ability to seal to all of them — turning a one-line `curl` into a client with a key-fetch, a trust decision about the key list the server hands it, and a re-key path. **This is the design's hardest tension**, because "a CI pipeline pushes `state=open` with `curl`" is the product's headline use case. A workable compromise: confidential channels accept publishes only from principals running the `notify` CLI, which can do the sealing, and the API refuses a plaintext publish to such a channel outright.
- **Only mobile devices have keys.** Browsers, desktop and the CLI enrol as transports with no client-side crypto at all. Confidential content means every client needs a keypair: a non-extractable `CryptoKey` in IndexedDB for the browser, OS keychain for desktop, a file with the right mode for the CLI. Each has a different loss and rotation story.
- **Recovery is a policy decision, not a technical one.** If the server cannot read content, then a user who loses every device loses their history permanently. The alternatives are a printable recovery key (users lose these), an escrowed key held by the platform (which reintroduces exactly the trust you removed, and should then be stated plainly rather than marketed as E2EE), or accepting the loss. Pick one *before* writing code, because it determines the key hierarchy.
- **Metadata still leaks, and always will.** Even with perfect content encryption the server knows who published, to which channel, when, how often, at what priority, how large, and who read it and when — the receipts and readership features are built on that. For many threat models the metadata *is* the sensitive part. Say so in the feature's documentation rather than letting "encrypted" be heard as "invisible".
- **Compliance rarely wants E2EE.** Organisations that ask for encrypted notifications usually mean "encrypted at rest with a key we control and can audit", and frequently also need retention, legal hold and administrative recovery — all of which E2EE forbids. Find out which one is being asked for before building the harder thing.

### 11.5 A three-tier path, and which tier I would build

**Tier 1 — envelope encryption at rest, server-readable. Build this first.**
`title`, `body`, action labels and attachment bytes encrypted with a per-notification
data key, itself wrapped by a key held in a KMS, Vault or a mounted key file.
Search, sinks, email, thumbnails and Web Push all keep working, because the server
still decrypts on read.

This closes the gap ADR-0007 currently leaves wide open — `pg_dump` output, disk
snapshots, a stolen backup tape, an object store misconfigured to public — which is
the realistic breach for a self-hosted product, far more so than a malicious
operator. It is a schema migration, a `ContentCipher` port with one adapter, and key
rotation. It is weeks, not quarters, and it is the change that would most improve the
security posture per unit of effort. It would also let `hardening.mdx` stop saying
"encryption at rest is your job".

**Tier 2 — a confidential notification kind, server-blind.** This is the "new kind
of notification" the question asks about. A per-channel flag, `confidential: true`,
set in GitOps configuration and immutable thereafter. On such a channel: content is
sealed client-side to member device keys; the server stores an opaque blob and the
plaintext lifecycle; search, email and webhook sinks are refused at configuration
time rather than silently degraded; push is always a poke; the API rejects a publish
that arrives unsealed. Start with sender keys and re-key on membership change, and
be explicit in the ADR that there is no forward secrecy yet.

**Tier 3 — MLS, forward secrecy, post-compromise security.** Only if a real threat
model demands it. It is a different product.

The two-tier split matters for a reason beyond effort: **Tier 1 protects everyone
automatically, Tier 2 protects only the people who opt in and accept the lost
features.** A security property that is on by default and invisible is worth more in
aggregate than a stronger one that a handful of channels enable.

### 11.6 Decide these before writing any of it

An ADR for this should answer, in order:

1. Which tier is actually being asked for — "encrypted at rest under our key" or "the server cannot read it"? They have opposite implications for recovery and compliance.
2. Is `confidential` a property of a channel or of a single notification? Per-channel is far simpler: the invariant is checkable at publish time and cannot be got wrong per-message.
3. What happens to content already stored when a channel is switched on? Migrating is a re-encryption job; refusing the switch on a non-empty channel is one line.
4. Who can publish to a confidential channel, given that a bare `curl` cannot?
5. What is the recovery story, stated as a promise to users rather than as an implementation detail?
6. Which metadata is accepted as visible, written down explicitly?
7. What is the key rotation procedure, and what does a compromised device key mean for history already delivered?
8. How is this tested? Sealed content makes most existing assertions unreadable, so the test doubles and fixtures need a plan of their own — and given how much of this codebase's correctness argument rests on its tests, that is not a footnote.

A last note on sequencing: none of this should start before the items in §8. An
encrypted body is worth very little on an instance that can be taken down by §4.1 or
served attacker HTML by §4.4, and Tier 1 in particular is a much better use of the
same weeks than either.

## 12. TODO — every point in this report

Every actionable item from §2 to §11, as a checklist. Each carries a stable id so it
can be referenced from a commit or an issue (`fix(security): … (SEC-P0-01)`), the
file to touch, and the section that argues for it.

Items marked **[decision]** are not work — they are a choice somebody has to record,
and recording "we accept this, because…" closes them just as well as changing code.
Items marked **[verify]** are hypotheses from §10 that need testing before they are
either fixed or dismissed.

### P0 — before tagging v0.1.0

<Callout>
  **Status: 9 of 11 done** (2026-08-10, commit `0138581`). `SEC-P0-08` and
  `SEC-P0-09` remain and are the same item twice — the release has to be rehearsed
  on a throwaway pre-release tag before anyone can read a real `audit:image` result,
  and pushing a tag is the maintainer's to do. Everything else below is implemented,
  with `task lint:check` clean and 1017 tests passing.
</Callout>

- [x] **SEC-P0-01** — Sign the content type into the filesystem upload token. Done as data on the `Purpose::Upload` variant rather than a parameter beside it, so the compiler refuses to mint or check an upload token without one. → `crates/notify-adapters/attachments-fs/src/token.rs` (§4.4)
- [x] **SEC-P0-02** — Refuse a PUT whose `Content-Type` disagrees with the token, and re-apply the allowlist at the upload route. The disagreement is caught *by the MAC itself* — the declared type is fed through the same digest — so there is no separate comparison to forget. The allowlist runs first and answers `415`, before the token is consulted. → `crates/notify-adapters/http-actix/src/routes/attachments_fs.rs`, allowlist now shared via `notify_app::{normalised_content_type, is_allowed_attachment_type}` (§4.4)
- [x] **SEC-P0-03** — `X-Content-Type-Options: nosniff` and `Content-Disposition: attachment` on all three byte-serving routes, through one shared `routes::served_bytes` helper so they cannot drift apart. → `attachments_fs.rs`, `attachments.rs`, `profile.rs` (§4.4)
- [x] **SEC-P0-04** — The module doc now describes what the code does, and says the guarantee is the signature rather than a check beside it. → `crates/notify-adapters/attachments-fs/src/lib.rs` (§4.4)
- [x] **SEC-P0-05** — Four regression tests, plus two at the token layer and one in the adapter conformance suite. Each was confirmed to **fail against the pre-fix code** before being kept. → `crates/notify-adapters/http-actix/tests/api.rs`, `attachments-fs/src/token.rs`, `attachments-fs/tests/conformance.rs` (§4.4)
- [x] **SEC-P0-06** — **Apache-2.0**, propagated to all declarations. Note there were **five**, not the four this report first counted: the OpenAPI document's own `info.license` also said MPL-2.0, and it is the one API consumers actually read. → `Cargo.toml`, `Containerfile`, `web/Containerfile`, `charts/weebo-notify/Chart.yaml`, `crates/notify-server/src/openapi.rs`; `LICENSE.md` was already Apache-2.0 and is unchanged (§6.1)
- [x] **SEC-P0-07** — `deny.toml`'s rationale rewritten against Apache-2.0. `MPL-2.0` deliberately **stays on the allow-list**: it is file-level copyleft, so shipping a dependency under it is fine, which is a different question from what we license our own work under. `cargo deny check licenses` passes. → `deny.toml` (§6.1)
- [ ] **SEC-P0-08** — Rehearse the release on `v0.1.0-rc.0` so both images are built for the first time on a runner, not on a tag that matters. **Needs you: pushing a tag is not something this review should do.** → `.forgejo/workflows/release.yaml` (§3.1)
- [ ] **SEC-P0-09** — Read and act on the first real `task audit:image` output; blocked on SEC-P0-08. → `.tasks/audit.yaml` (§3.1)
- [x] **SEC-P0-10** — Both `cosign sign` calls now take `IMAGE@${{ steps.<id>.outputs.digest }}`, with the build steps given ids and the job failing closed if either digest comes back empty. → `.forgejo/workflows/release.yaml` (§6.2)
- [x] **SEC-P0-11** — Implemented rather than accepted: the release is created from the `cog changelog --at` section, carries both image digests and the `cosign verify` command that checks them, attaches both SBOMs, and is idempotent on re-run. Targets the forge explicitly (`GH_HOST`/`GH_REPO`) because Forgejo is primary here and GitHub is a mirror, and uses the runner's own token rather than `REGISTRY_TOKEN` — pushing an image and editing a release are different authorities. **Unverified: this step has never run.** → `.forgejo/workflows/release.yaml` (§6.2)

### P1 — before, or immediately after, the release

<Callout>
  **Status: 15 of 15 done** (2026-08-10). One resolved as not-actionable with its
  reasoning recorded — see SEC-P1-12 — and one implemented differently from the way
  this report first proposed it, for a reason the original text had wrong: see
  SEC-P1-04.
</Callout>

**The authentication hot path (§4.1)**

- [x] **SEC-P1-01** — Done as a memoisation of the KDF verdict, not of the caller. `SecretHasher::verify(secret, hash)` is a pure function, so remembering it has *no* semantic effect — expiry, revocation and account deactivation are still read from the database on every request, and a revoked key stops working immediately rather than at the end of a TTL. That is why there is no TTL at all: a bounded LRU is the whole mechanism. → `crates/notify-adapters/oidc/src/guard.rs`
- [x] **SEC-P1-02** — Negative verdicts cached in a **separate** LRU from the positive ones. One cache would have let an attacker walk a few thousand wrong secrets through it and evict every legitimate entry, putting the KDF back on the hot path for everybody — paying for the attack twice. There is a test for exactly that. → same
- [x] **SEC-P1-03** — Implemented as **admission control rather than address-keyed rate limiting**, deliberately: behind an ingress the peer address is the ingress and the forwarded one is a header the client sets, so limiting on it without a configured proxy list is a way to frame somebody else. Instead, verifications past a queue ceiling are shed with a `503` rather than queued without bound. Address-keyed limiting is documented as the ingress's job (SEC-P1-07). → `guard.rs`, `hardening.mdx`
- [x] **SEC-P1-04** — **Not `max_blocking_threads`; this report was wrong about that being reachable.** `actix-web` gives each worker its own `actix-rt` Arbiter with its own current-thread runtime and its own blocking pool, so there is no single pool to cap — `System::with_tokio_rt` sets the system runtime's, not the workers'. A semaphore in front of the KDF bounds the same resource without depending on runtime internals, and bounds it more tightly: concurrency × 19 MiB, counted over the calls that matter rather than every blocking task in the process. → `guard.rs`
- [x] **SEC-P1-05** — `last_used_at` refreshed at most once a minute per key. A key never used before is always recorded (that is the write somebody is actually waiting for after minting one), and a clock that steps backwards writes rather than going quiet. → `crates/notify-app/src/auth.rs`
- [x] **SEC-P1-06** — Written down, with the consequence spelled out: do not build a control on the prefix being private. → `docs/content/docs/ops/hardening.mdx`
- [x] **SEC-P1-07** — A section on what belongs at the ingress: request rate, **connection** count (which matters more than usual, since SSE and WebSocket connections are long-lived by design), and body size, with the ingress-nginx annotations named. → same

**Chart and operations**

- [x] **SEC-P1-08** — The production example now sets `ingressFrom` for both the ingress-controller and the monitoring namespace, as clearly-marked placeholders. Verified by rendering the chart and reading the resulting `NetworkPolicy`: the server now admits the ingress controller and Prometheus alongside the `web` pods. → `charts/weebo-notify/examples/values-production.yaml` (§5.1)
- [x] **SEC-P1-09** — Sizing section with the arithmetic: 16 per replica plus one `LISTEN` connection outside that pool, × `maxReplicas: 6` ≈ 102 before anything else connects, and a note that a rolling update briefly runs both generations. → `docs/content/docs/ops/high-availability.mdx` (§7.3)
- [x] **SEC-P1-10** — A warning Callout on transaction-mode pooling, including *how it fails*: writes keep working and `/readyz` stays green while real-time delivery silently stops. → same (§7.3)

**Gates and dependencies**

- [x] **SEC-P1-11** — Both commands added. It found something immediately: `weebo-notify-desktop` declared **no licence at all** — it cannot inherit `workspace.package.license`, being deliberately excluded from the workspace — so it is now Apache-2.0 like everything else. → `.tasks/audit.yaml`, `web/src-tauri/Cargo.toml` (§2.2)
- [x] **SEC-P1-12** — **Resolved as not actionable, and the report's framing was too strong.** `cargo audit` reports **zero vulnerabilities** in that lockfile: `glib` 0.18.5 is RUSTSEC-2024-0429, classed *unsound*, and it arrives with the whole `gtk-rs` GTK3 binding set, all of which is *unmaintained* upstream. It cannot be bumped from here — `gtk 0.18.2` is pinned by Tauri 2.11.5 itself, through `tao`, `wry`, `webkit2gtk`, `muda` and `libappindicator` — so "bump to ≥ 0.20" is not a move that exists until Tauri leaves GTK3. Nothing builds or ships this crate today (M5). The lockfile was deliberately **not** blind-updated: no gate can compile it, so an unverifiable resolution change would be exactly the "committed but never executed" state this report objects to elsewhere. **Revisit when** Tauri's Linux stack moves off GTK3, or if any of these becomes a vulnerability rather than a warning. → `web/src-tauri/Cargo.lock` (§2.2)
- [x] **SEC-P1-13** — Removed outright. Since openapi-ts v0.73 the client is bundled and emitted into `src/api/generated/client/`; nothing imported the package, so it was dead weight in `dependencies`. The plugin name in `openapi-ts.config.ts` is unchanged and still resolves. → `web/package.json` (§2.3)
- [x] **SEC-P1-14** — `lucide-vue-next` → `@lucide/vue` across 13 files. Pinned to `^1.28.0` rather than the latest: `1.31.0` had been published **the previous day**, and taking it required an exclusion from the `minimumReleaseAge` supply-chain policy that pnpm added automatically. That exclusion was reverted and the lockfile rebuilt so the gate chose `1.30.0` on its own — a freshly-published version is the exact thing that policy exists to refuse, and silently excluding one to make an install succeed defeats it. → `web/package.json`, `web/pnpm-workspace.yaml` (§2.3)
- [x] **SEC-P1-15** — `EXPOSE 8080`, and the false comment replaced with what actually keeps the exporter private (the Ingress path prefix) and what breaks it. → `Containerfile` (§3.3)

### P2 — hardening

<Callout>
  **Status: 42 of 43 done** (2026-08-10). Two were resolved as *not actionable* with
  their reasoning recorded rather than quietly ticked — SEC-P2-04, where the
  restructure cannot be verified against a pipeline that has never run, and
  SEC-P2-15c, where the jitter this report asked for already existed. Two were
  narrowed honestly: SEC-P2-26 ships a digest, not the signature that needs M5's key
  custody, and SEC-P2-30 covers the accounting a unit test can reach but not the
  memory behaviour that needs a load rig. Four `[decision]` items were answered by
  the maintainer and are marked as decisions in place.

  **SEC-P2-39 stays open on purpose**: commissioning a penetration test is not
  something this review can do, and not something it should pretend to have replaced.
</Callout>

**Supply chain (§6.3, §10 A3)**

- [x] **SEC-P2-01** — Every action in all nine workflow files pinned to a full commit SHA, with the tag kept as a trailing comment. Renovate (SEC-P2-06) is configured with `pinDigests` so the pins move deliberately rather than rotting. → all files under `.forgejo/workflows/` and `.github/workflows/`
- [x] **SEC-P2-02** — All four base images pinned by manifest digest — `rust:1.97-slim-bookworm`, `distroless/cc-debian12:nonroot`, `node:24-slim`, `nginx-unprivileged:1.29-alpine`. Digests resolved from the registries and kept beside the tag, so a reader still sees which version it is. → `Containerfile`, `web/Containerfile`
- [x] **SEC-P2-03** — `curl https://mise.run | sh` is gone from all eight steps that ran it. They now fetch a pinned release binary and verify its SHA-256 before executing it — the whole point being that these steps run in jobs holding `REGISTRY_TOKEN` and the signing identity. Verified by running the exact sequence: checksum `OK`, binary reports `2026.8.3`. → every workflow's "Install the toolchain" step
- [x] **SEC-P2-04** — **Resolved as not actionable without an unverifiable restructure, and the report overstated what was available.** Splitting build from push means building to an OCI archive, uploading it as an artefact, and pushing from a second job — and `cosign` then needs registry write in *that* job too, so the credential does not actually leave the pipeline, it moves. Against a release path that has still never run, that is a change I cannot verify and would not want discovered on the first real tag. What is true today and worth stating: `REGISTRY_TOKEN` is referenced only in the `with:` of the login step (step-scoped, not exported), every toolchain and tool installation now happens *before* that login, and everything those steps fetch is SHA- or digest-pinned and checksum-verified (SEC-P2-01…03). **Revisit when** the release pipeline has run at least once and there is a known-good baseline to restructure against. → `.forgejo/workflows/release.yaml` (§6.3, §10 A3)
- [x] **SEC-P2-05** — Documented as a publishing right rather than a credential: scope it to the package not the account, give it to the release workflow alone, rotate on a schedule and on any permissions change, and prefer the forge's short-lived job token. Also written down is why signing does not make it less important — a stolen token can push, it just cannot produce something the admission policy accepts.
- [x] **SEC-P2-06** — `renovate.json`, covering cargo, pnpm, Dockerfile bases and CI actions in one config. Vulnerability alerts bypass the weekly window; the unbuildable desktop workspace is grouped separately and flagged for manual verification; a custom manager keeps the pinned mise version current. → new `renovate.json` (§2.4)
- [x] **SEC-P2-07** — A `ClusterImagePolicy` example in `hardening.mdx` with both halves of the identity constrained — without `issuer` any Fulcio certificate satisfies it, without `subject` any workflow in any repository does — plus the by-hand `cosign verify` and a Callout on pinning deployments to digests rather than tags. → `docs/content/docs/ops/hardening.mdx`
- [x] **SEC-P2-08** — `claude`, `rtk` and `herdr` moved to `mise.dev.toml`, which loads only under `MISE_ENV=dev`; `task init` sets it. Verified that the split works: without the variable the file is not read. CI now installs neither, so three third parties leave the blast radius of the job that holds the registry token. → `mise.toml` (§6.4)

**Credentials and scanners**

- [x] **SEC-P2-09** — `UlidKeyGenerator` → `OsRngKeyGenerator`: 8-character prefix and 52-character secret drawn straight from the OS CSPRNG, with rejection sampling so the 36-character alphabet is unbiased (`byte % 36` would have made four letters ~14% likelier). Five tests, including one that asserts every character of the alphabet is reachable — an off-by-one in the rejection ceiling would otherwise cost entropy silently. → `crates/notify-app/src/keys.rs:341` (§4.3)
- [x] **SEC-P2-10** — Four test-fixture paths allowlisted with the reasoning. `gitleaks detect` over all 27 commits now reports **no leaks**, so a seventh finding is a signal rather than noise. → `.gitleaks.toml` (§4.6)
- [x] **SEC-P2-11** — All three written into `charts/weebo-notify/.trivyignore` with reasons and revisit conditions. KSV-0011 is recorded as a considered disagreement rather than a false positive: a CPU limit throttles under burst, and the memory limit is what actually bounds a runaway pod. → `charts/weebo-notify/.trivyignore` (§5.4)
- [x] **SEC-P2-12** — Done, and it required a second ignore file: `.trivyignore` at the repository root, carrying the `glib` advisory (SEC-P1-12's unfixable upstream pin) and the test JWT. Both gates now run at **MEDIUM** and pass. That is the point — a threshold above a finding cannot distinguish "understood" from "unseen". → `.tasks/audit.yaml` (§5.4)

**Chart defaults and documentation**

- [x] **SEC-P2-13** — `NOTES.txt` prints it at install time, naming what it costs: every upgrade and every node drain is a full outage for the API and every live stream. Verified by rendering with `attachments.kind=filesystem`. → `charts/weebo-notify/templates/NOTES.txt` (§5.3, §7.2)
- [x] **SEC-P2-14** — Hard `topologySpreadConstraints` on `kubernetes.io/hostname` plus `podAntiAffinity: hard` in the production example, so a cluster that cannot spread the replicas says so at deploy time rather than at 3am. Verified in the rendered Deployment. → `charts/weebo-notify/examples/values-production.yaml` (§7.4)
- [x] **SEC-P2-15** — `behavior.scaleDown` with a 300 s stabilisation window and a one-pod-per-120 s policy, plumbed through the HPA template and defaulted in `values.yaml`. Scale-up is left fast. → `charts/weebo-notify/templates/server-hpa.yaml`, `values.yaml` (§7.4)
- [x] **SEC-P2-15b** — A worked `customMetrics` example for `sse_connections` in `values.yaml`, with the reason it matters: a replica can sit at 5% CPU while holding every stream it can carry. → `charts/weebo-notify/README.md`, `docs/content/docs/ops/high-availability.mdx` (§7.4)
- [x] **SEC-P2-15c** — Jitter was **already there** and I was wrong to list it as missing: `web/src/api/stream.ts` waits `backoff × (0.5 + random/2)`, with the reason written beside it. The CLI has no stream client yet (M2), so there is no second place to fix. The measurement half stays open — it needs a load rig — but the herd is now bounded from the other end by the per-replica ceiling of SEC-P2-31. → `web/src/`, `crates/notify-cli/`, `docs/content/docs/ops/high-availability.mdx` (§7.4)
- [x] **SEC-P2-16** — Its own section in `hardening.mdx`, listing the three routing changes that would publish the exporter and what it discloses when published. → `docs/content/docs/ops/hardening.mdx` (§5.2)
- [x] **SEC-P2-17** — **Decision: a toggle, defaulting on.** `[server] docs` (chart: `server.config.server.docs`). Off *unmounts* the route rather than answering `401`, because an endpoint that exists and refuses still confirms it is there. → `crates/notify-server/src/main.rs:209` (§4.5)
- [x] **SEC-P2-18** — **Decision: keep `false`.** Default-on with an unset `ingressFrom` blocks the API, and the NOTES warning only reaches people who read install output — a breaking upgrade for existing installs is too high a price before 1.0. The production example shows the correct shape and `task helm:check` now asserts it. → `charts/weebo-notify/values.yaml` (§10 A4)
- [x] **SEC-P2-19** — New `task helm:check`, wired into the `check` workflow. It renders the production example and asserts five properties an operator inherits from it — NetworkPolicy sources, replica count, PDB, HPA scale-down window, and that the Ingress publishes `/api/v1` and nothing else. Confirmed to **fail** when `ingressFrom` is emptied, which is the exact regression that shipped. → `.forgejo/workflows/check.yaml`, `.tasks/helm.yaml` (§10 A4)

**Operator-visible safety (§10 A4)**

- [x] **SEC-P2-20** — `crates/notify-server/src/doctor.rs`: a configuration review that runs at boot and logs each finding. It checks what a process can know about itself — wildcard CORS, a placeholder or short `token_secret`, an `http://` presign origin, the filesystem backend's HA cost — and then **names what it cannot check**, so the output is a complete list of what to verify rather than a complete list of what was verified. → `crates/notify-server/src/wiring.rs`
- [x] **SEC-P2-21** — `notify-server doctor`: the same review, on demand, exiting non-zero on any warning so it works as a deployment gate. It connects to nothing, so it is safe against a live instance and its output is safe to paste into an issue. Run against `config/dev.toml` it reports four notes and no warnings; with `WNK_SERVER__CORS_ORIGINS='["*"]'` it warns and exits 1. → `crates/notify-server/src/main.rs`
- [x] **SEC-P2-22** — `hardening.mdx` opens with a nine-item checklist linking to each section, and `NOTES.txt` points at it on every install. → `docs/content/docs/ops/hardening.mdx`, `charts/weebo-notify/templates/NOTES.txt`

**Frontend (§10 A5)**

- [x] **SEC-P2-23** — All four `v-html` sites replaced with `<i18n-t>` slots, so markup lives in the template and a translation is inert data. It fixed a real bug on the way: `settings.push.description` interpolated a hardcoded English `<strong>device</strong>` into the French string, and the emphasis words are now translated (`appareil`, `urgentes`). → `web/src/components/settings/PushPanel.vue:69`, `web/src/components/settings/DndPanel.vue:253,268,301`
- [x] **SEC-P2-24** — `web/src/locales/locales.spec.ts` — nine assertions that no locale value contains a tag, an entity that decodes to one, or a `javascript:`/`data:` URL, plus key parity between locales. Confirmed to fail on an injected `<img src=x onerror=…>`. It reads the files off disk rather than importing them, because `unplugin-vue-i18n` pre-compiles messages to an AST and even `?raw` goes through it. → same, plus `web/src/locales/`
- [x] **SEC-P2-25** — `CODEOWNERS`, with `web/src/locales/` listed alongside the credential and supply-chain paths — reviewers read code diffs carefully and locale diffs quickly, which is what makes a locale file a good hiding place. → `CODEOWNERS`

**The CLI as a distribution channel (§10 A6)**

- [x] **SEC-P2-26** — Partially, and the honest half: `GET /api/v1/cli` now serves `X-Content-Digest: sha-256=…` and a strong `ETag`, hashed once at boot, plus `nosniff`. That lets a recipient check the executable against a value published out of band. It is **not a signature** and is not presented as one — a detached minisign signature and self-update verification need the key custody that lands with the desktop updater in M5. → `crates/notify-adapters/http-actix/src/routes/cli.rs`, `crates/notify-cli/`
- [x] **SEC-P2-27** — The server logs the digest at startup and serves it on the download; the release notes already carry the image digests (SEC-P0-11). Publishing the *CLI archive* digest waits on M5, when there is a CLI artefact in a release to publish one for. → `.forgejo/workflows/release.yaml`
- [x] **SEC-P2-28** — Documented as a code-execution surface for every machine that runs `notify`, with the note that the binary currently ships without a detached signature. → `docs/content/docs/ops/configuration.mdx`
- [x] **SEC-P2-29** — **Decision: leave it open to any authenticated caller.** The bytes are the same ones in the public release, so a leaked key gains nothing an attacker could not download anyway. Recorded rather than changed. → `crates/notify-adapters/http-actix/src/routes/cli.rs`

**Streams (§10 A7)**

- [x] **SEC-P2-30** — Done to the extent it can be without a load rig: four tests in `slots.rs` cover the accounting under churn — fill to the ceiling, abandon, repeat, interleaved — and assert the total returns to zero every time. A leaked total would refuse every connection on the replica until restart, invisibly until it was total. What still needs a real rig is the *memory* behaviour under socket-level abandonment, which no unit test can observe. → `crates/notify-adapters/http-actix/src/routes/stream.rs`
- [x] **SEC-P2-31** — `MAX_STREAMS_PER_REPLICA` (10 000) alongside the per-principal cap, with the total maintained under the same lock rather than summed per connect. The per-principal cap bounds what one caller costs; nothing bounded what the replica cost, so four hundred entirely legitimate principals could exhaust the process. Refusing the ten-thousand-and-first connection costs that client a reconnect; being OOM-killed costs every connection on the replica.
- [x] **SEC-P2-32** — Covered by SEC-P2-31 rather than separately: the gauge that feeds the HPA now cannot exceed the ceiling, so the cost-attack it describes is bounded by construction. A refusal is also asserted to allocate nothing — a refusal path that left a counter or map entry behind would itself be the attack.

**Process (§10.2)**

- [x] **SEC-P2-33** — `SECURITY.md`: contact, a five-working-day acknowledgement, what is in and out of scope, a pointer to every accepted-finding file, and the `cosign verify` command for a release. → `SECURITY.md`
- [x] **SEC-P2-34** — [ADR-0023](/docs/adr/0023-the-threat-model). Seven adversaries in scope with the control that answers each, six explicitly out — including the operator, who reads everything — and the six deployment-owed guarantees, with the observation that every one of them fails silently. It ends with a rule for using it: name the adversary when adding a control or suppressing an advisory. → new ADR
- [x] **SEC-P2-35** — `crates/notify-domain/tests/parsers_survive_hostile_input.rs`: 20 000 rounds of hostile input against all five hand-written parsers, on a fixed seed so a failure reproduces exactly. Asserts they return at all, and that anything accepted round-trips — a parser that accepts what it will not emit is where two components come to disagree about the same string. Runs in 0.11 s on every `cargo test`. Deliberately **not** `cargo-fuzz`: that needs a nightly toolchain this repository does not pin, which would make the targets one more artefact committed and never executed. This is the floor; add fuzz targets over the same entry points when nightly is available. → new `fuzz/` directory, `.forgejo/workflows/audit.yaml`
- [x] **SEC-P2-36** — A rotation table in `hardening.mdx` covering all seven operator-held secrets, with a Callout on the two that behave unlike ordinary credentials — rotating `token_secret` invalidates every presigned URL already in the world, and rotating the VAPID key silently unsubscribes every browser. → `docs/content/docs/ops/hardening.mdx`
- [x] **SEC-P2-37** — **Decision: append-only, never purged**, which turned out to be the existing behaviour — no `DELETE` in the persistence crate names `audit_entries`. `tests/audit_is_append_only.rs` asserts it against the source, so the property survives a refactor: one test refuses any destructive statement against the table, the other pins the TTL purge to notifications alone. → `crates/notify-app/src/maintenance.rs`, migrations
- [x] **SEC-P2-38** — `crates/notify-app/tests/authorisation_boundaries.rs`: nine tests, each naming the escalation it prevents — lateral movement between channels, a key inheriting its owner's rights, group claims widening a key, service-account/user confusion, the platform role as a channel key, and refusals that reveal whether a channel exists. Verified by widening the grant check on purpose: two of the nine fail immediately, which is the point. → `crates/notify-app/src/authz.rs` tests, `crates/notify-adapters/http-actix/tests/api.rs`
- [ ] **SEC-P2-39** — **Yours: commission a penetration test.** Not something this review can do or substitute for, and the one item here that is deliberately left open rather than resolved. Everything above is code read by an assistant and tests it wrote; a person with a proxy, a week and no prior belief about how the system works finds a different class of thing. Worth doing before this instance is trusted with anything sensitive — [ADR-0023](/docs/adr/0023-the-threat-model) is the document to hand them. (§10.2)
- [x] **SEC-P2-39b** — Reviewed. **No findings.** PKCE is S256 and discovery *refuses* a provider that does not advertise it; `code_verifier` is 32 CSPRNG bytes and `state` 16, checked on callback as the CSRF gate; the access token stays in memory and the refresh token in `sessionStorage` as a documented decision with its exposure stated. `id_token` claims are read without signature validation — correct here, because they came from the token endpoint over TLS and drive **display only**: the name in the header and a read-only group list in Settings. Every authorisation decision is the server's, against JWKS. Worth keeping true — the moment a claim gates something security-relevant in the client, that token needs validating. A `nonce` is absent and is not required for code-flow-with-PKCE; adding one without id_token signature validation would be theatre. → `web/src/`
- [x] **SEC-P2-40** — `task audit:suppressions`, in the default audit run and therefore in the nightly job. It asks the one question a machine can answer about each ignore — is the fixed version reachable now? — and reports `quick-xml 0.38.4 — still <0.41`, `rsa 0.9.10 — check for a patched release`, `gtk 0.18.2 — Tauri still on GTK3`. Advisory, never a gate: a suppression that has become removable is good news and must not turn the build red. → `.cargo/audit.toml`, `deny.toml` (§2.1)

### Beyond this release

- [ ] **SEC-FUT-01** — Envelope-encrypt notification content at rest under an operator-held key (§11.5, Tier 1): a `ContentCipher` port, one KMS/Vault/key-file adapter, a schema migration, and key rotation. Closes the gap ADR-0007 leaves open, keeps search, sinks, email and Web Push working, and is the largest security gain per unit of effort in this report.
- [ ] **SEC-FUT-02** — Once Tier 1 lands, update `hardening.mdx` to stop saying encryption at rest is entirely the deployment's job.
- [ ] **SEC-FUT-03** — **[decision]** If a server-blind notification class is wanted, open an ADR answering all eight questions in §11.6 *before* any code — most importantly which tier is actually being asked for, and what the recovery promise to users is.
- [ ] **SEC-FUT-04** — If Tier 2 proceeds: give browser, desktop and CLI clients keypairs; add the `confidential` channel flag with publish-time enforcement; refuse email and webhook sink pairings on such channels at config-validation time; force `PushBody::Poke`; document the metadata that remains visible.

### Not from this review, but blocking its publication

<Callout>
  **Done.** This page can now be published — the docs site builds all 129 pages,
  including this one.
</Callout>

- [x] **SEC-MISC-01** — Fixed, and fixed at the mechanism rather than at the fence. One ```promql block failed `next build` for all 129 pages: Shiki bundles 723 grammars and promql is not one of them. Relabelling that fence would have worked until the next unknown language did the same thing, so the docs pipeline now sets `rehypeCodeOptions.fallbackLanguage: "text"` — an unrecognised language renders as plain text instead of throwing. Verified both ways: the site builds with the file in place, and an added ```totally-not-a-language fence builds clean too. The fence keeps its `promql` label, which nothing displays (`addLanguageClass` is off for every block on this site) but which is what a grammar would attach to later. → `docs/src/lib/source.ts`
