# The deployable server (design §3.2bis, §14.4/D17).
#
# Until this file existed, `release.yaml` emitted `::warning::Image build is not
# wired yet` and a tag produced nothing anybody could run. That is the hole D17
# names, and it is wider than the CSP headers that led to finding it.
#
# NOT VERIFIED HERE. This repository is developed in an Eclipse Che workspace with
# no container engine (§12.4), so the first build of this file happens on the
# Forgejo runner. That is written down rather than glossed: it is the same
# "committed but never executed" state D7 and D12 are open for, and it is a
# deliberate trade — the alternative was leaving the release producing nothing.

# ─────────────────────────────────────────────────────────────────────────────
# Build
# ─────────────────────────────────────────────────────────────────────────────
# Pinned to the toolchain this workspace develops on rather than floating on `1`:
# a release image is the one artefact that should not change because a base tag
# moved between two runs of the same pipeline. `trixie` is not a preference — it
# has to be the same Debian as the runtime stage below (`cc-debian13`), or the binary links against
# a glibc the runtime does not have.
FROM docker.io/library/rust:1.99-slim-trixie@sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d AS builder

WORKDIR /build

# The whole workspace, in one layer, and the dependency-stubbing trick is
# deliberately NOT used. It works by copying every `Cargo.toml`, stubbing every
# `lib.rs`/`main.rs`, and building that first so dependencies land in their own
# layer. With twenty-one crates it is twenty-one paths to keep in step with
# `Cargo.toml`, and when somebody adds the twenty-second nothing fails — the cache
# just quietly stops helping, which is the kind of rot nobody notices for a year.
# A tag build is not on anybody's critical path.
COPY . .

# `--locked` so the image is built from the committed lockfile and a release cannot
# silently pick up a version that was never tested. The release job is exactly where
# a surprise resolution is least welcome.
#
# `notify-cli` is built in the same invocation, from the same commit, as
# `notify-server` — the whole reason `GET /api/v1/cli` (design §9) can hand out
# this build's binary and never disagree with the server serving it. Linux only
# for now: the cross-platform artefact matrix is M5's job.
RUN cargo build --release --locked -p notify-server -p notify-cli

# ─────────────────────────────────────────────────────────────────────────────
# Runtime
# ─────────────────────────────────────────────────────────────────────────────
# `cc` and not `static`: the binary is glibc-linked. It is also all that is needed —
# the TLS stack is rustls throughout (`Cargo.toml`: sqlx `tls-rustls`, reqwest
# `reqwest-rustls`, rust-s3 `tokio-rustls-tls`, lettre `tokio1-rustls-tls`), so
# there is no libssl to carry and no OpenSSL CVE to inherit. The one `openssl`
# name in `Cargo.lock` is `openssl-probe`, which reads a certificate path.
#
# `nonroot` is the tag, not a runtime flag: the image has no shell, no package
# manager and no root user to drop from.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS runtime

COPY --from=builder /build/target/release/notify-server /usr/local/bin/notify-server

# The CLI binary `GET /api/v1/cli` (design §9) hands out. Present in every image,
# same as `notify-server` above — but nothing serves it until an operator opts in
# with a `[cli]` section pointing at this path, on the same "no default
# configuration is baked in" terms the volume below states.
COPY --from=builder /build/target/release/notify /usr/local/bin/notify

# Where an operator mounts `config.toml` and the secrets it points at (§11). No
# default configuration is baked in: a server that starts on a config nobody wrote
# is a server pointing at somebody else's idea of a database.
VOLUME ["/etc/weebo-notify"]

# 8080, and only 8080. This used to also expose 9090 and describe it as "the
# Prometheus exporter, a separate listener so it can stay off the public ingress",
# which was not true of any build of this image: `notify-server` binds one socket,
# and `/metrics`, `/healthz` and `/readyz` all answer on it beside the API.
#
# Worth correcting rather than leaving as an aspiration, because it is the kind of
# claim an operator builds a network policy on. What actually keeps the exporter
# private today is the Ingress, which routes `/api/v1` and nothing else — so a
# deployment that fronts this Service with a catch-all path, a second Ingress or a
# LoadBalancer publishes the exporter along with the API. `hardening.mdx` says so
# too. The second listener remains a reasonable thing to build; when it exists,
# this line and that paragraph change together.
EXPOSE 8080

# No HEALTHCHECK, and it is a decision rather than an omission: `trivy config`
# reports its absence at LOW, below the gate `task audit` enforces. A distroless
# image has no shell and no wget to run one with, and the server already serves the
# real answers on `/healthz` and `/readyz` (M1.8) — which is what a Kubernetes probe
# calls. Adding a check here would mean adding a binary to a runtime whose whole
# point is not having one.
LABEL org.opencontainers.image.title="weebo-notify"
LABEL org.opencontainers.image.description="Self-hosted notification hub — server"
LABEL org.opencontainers.image.source="https://github.com/batleforc/weebo-notify-v2"
LABEL org.opencontainers.image.licenses="Apache-2.0"

USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/notify-server"]
CMD ["--config", "/etc/weebo-notify/config.toml"]
