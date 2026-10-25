# The deployable server (design §3.2bis, §14.4/D17).
#
# Until this file existed, `release.yaml` emitted `::warning::Image build is not
# wired yet` and a tag produced nothing anybody could run. That is the hole D17
# names, and it is wider than the CSP headers that led to finding it.
#
# Built and scanned locally with podman on 2026-10-25 (`task build:image`, then
# `task audit:image`: 0 HIGH/CRITICAL), amd64 only. A Che workspace still has no
# container engine (§12.4), and CI is on hold until the Forgejo move, so the arm64
# cross build has not run yet.

# ─────────────────────────────────────────────────────────────────────────────
# Build
# ─────────────────────────────────────────────────────────────────────────────
# Pinned to the toolchain this workspace develops on rather than floating on `1`:
# a release image is the one artefact that should not change because a base tag
# moved between two runs of the same pipeline. `trixie` is not a preference — it
# has to be the same Debian as the runtime stage below (`cc-debian13`), or the binary links against
# a glibc the runtime does not have.
#
# `$BUILDPLATFORM`: the compiler always runs natively and cross-compiles to the
# target, so the arm64 image no longer builds the workspace under QEMU (hours, not
# minutes). The runtime stage has no `RUN`, so nothing in this file is emulated.
FROM --platform=$BUILDPLATFORM docker.io/library/rust:1.99-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS chef

ARG BUILDARCH
ARG TARGETARCH

# Every C dependency in the tree (aws-lc-sys, ring, zstd-sys, libsqlite3-sys) builds
# through `cc`, so a cross build needs only the target's gcc and libc headers.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gnu=x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$triple" > /rust-target; \
    rustup target add "$triple"; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$gnu" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi
# Read by cargo and `cc` only when that target is built; inert otherwise.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar

# cargo-chef derives the dependency-only build from the manifests by itself, which
# is what the hand-written stub trick could not do: a crate added to the workspace
# is picked up with no line to add here. It also normalises the workspace version,
# so a release bump does not invalidate the dependency layer on the one build that
# counts.
RUN cargo install --locked cargo-chef --version 0.1.78

WORKDIR /build

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Dependencies only: this layer is reused until a manifest or Cargo.lock changes.
RUN cargo chef cook --release --locked --target "$(cat /rust-target)" \
    -p notify-server -p notify-cli --recipe-path recipe.json

COPY . .

# `--locked` so the image is built from the committed lockfile and a release cannot
# silently pick up a version that was never tested. The release job is exactly where
# a surprise resolution is least welcome.
#
# `notify-cli` is built in the same invocation, from the same commit, as
# `notify-server` — the whole reason `GET /api/v1/cli` (design §9) can hand out
# this build's binary and never disagree with the server serving it. One CLI per
# image architecture; the cross-platform artefact matrix is M5's job.
RUN cargo build --release --locked --target "$(cat /rust-target)" -p notify-server -p notify-cli \
    && mkdir /out \
    && cp "target/$(cat /rust-target)/release/notify-server" "target/$(cat /rust-target)/release/notify" /out/

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
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e AS runtime

COPY --from=builder /out/notify-server /usr/local/bin/notify-server

# The CLI binary `GET /api/v1/cli` (design §9) hands out. Present in every image,
# same as `notify-server` above — but nothing serves it until an operator opts in
# with a `[cli]` section pointing at this path, on the same "no default
# configuration is baked in" terms the volume below states.
COPY --from=builder /out/notify /usr/local/bin/notify

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

# Numeric, the UID `nonroot` maps to: the kubelet can only verify `runAsNonRoot`
# against a number, and it is the one the chart's `runAsUser` pins.
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/notify-server"]
CMD ["--config", "/etc/weebo-notify/config.toml"]
