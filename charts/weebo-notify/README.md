# weebo-notify

A Helm chart for [weebo-notify](../../readme.md) — a self-hosted, multi-channel
notification platform. Deploys two Deployments/Services (`notify-server` and its
web bundle, two separate origins by design — [ADR-0021][adr-0021]) and nothing
else: Postgres, the object store, Redis and the OIDC issuer are external
dependencies this chart does not bundle, on the same terms
[docs/ops/high-availability.mdx][ha] and [docs/ops/hardening.mdx][hardening]
already state for the application itself — their HA and their encryption at
rest belong to whoever operates them.

[adr-0021]: ../../docs/content/docs/adr/0021-the-runtime-config-adr-0019-anticipated.mdx
[ha]: ../../docs/content/docs/ops/high-availability.mdx
[hardening]: ../../docs/content/docs/ops/hardening.mdx

## Before a first install

1. **A Postgres database.** Create a Secret with the DSN and point
   `server.database.existingSecret` at it (key: `server.database.secretKey`,
   default `database-url`).
2. **An OIDC issuer.** Set `server.oidc.issuer`/`clientId` and
   `web.env.oidcIssuer`/`oidcClientId` — they have to describe the same
   provider, since the browser and the API validate against it independently.
3. **An attachment store.** Either an S3-compatible bucket
   (`server.attachments.kind: s3` — the default) or a local directory
   (`server.attachments.kind: filesystem`, [ADR-0022][adr-0022] — see
   "Filesystem attachments" below, it changes the topology).
4. **`web.env.apiBaseUrl`.** The server's own public origin, e.g.
   `https://api.notify.example.com` — a bare origin, no path, no trailing
   slash (the CSP built from it cannot parse either). This chart never guesses
   it from `server.ingress.host`.

`helm install`/`helm template` fail outright on a missing `web.env.apiBaseUrl`
or `web.env.oidcIssuer` — the two values with no default that would fail
obviously at runtime instead. NOTES.txt (printed after install/upgrade) warns
on the rest of the checklist above, since Kubernetes itself has no way to
catch those for you.

## Installing

The chart is published as an OCI artifact next to the images, signed with the
same keyless cosign identity:

```bash
cosign verify ghcr.io/batleforc/charts/weebo-notify:0.4.2 \
  --certificate-identity-regexp '^https://github.com/batleforc/weebo-notify-v2/\.github/workflows/release\.yaml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
helm install notify oci://ghcr.io/batleforc/charts/weebo-notify --version 0.4.2 -f my-values.yaml
```

Chart `version` and `appVersion` both equal the product version; the image
tag defaults to `v<appVersion>` (the git tag). Pin `image.server.digest` /
`image.web.digest` to the digests in the release notes to deploy exactly the
bytes that were scanned and signed.

## Migrations

`migrations.job.enabled` (default `true`) runs `notify-server migrate` as a
`pre-install,pre-upgrade` hook Job, with the server's own env, secrets and
security context, and sets `WNK_DATABASE__MIGRATE_ON_START=false` on the
Deployment. A failing migration fails the `helm upgrade` and leaves the old
pods serving; the failed Job is kept for its logs. Without hooks
(`helm template | kubectl apply`), set it to `false` and the server migrates
on boot under Postgres's advisory lock; the startup probe
(`server.startupProbe`) gives that boot five minutes before liveness counts.
Migrations are expand/contract — see `docs/ops/runbooks`.

## Alerts and dashboard

`prometheusRule.enabled` ships alerts for leader loss, stale or failing
background jobs, sink dead letters, push failure ratio, retention backlog,
scrape loss, readiness flapping (kube-state-metrics) and — on ingress-nginx —
the API's 5xx ratio. `grafanaDashboard.enabled` ships one dashboard as a
ConfigMap for the Grafana sidecar. Both need `server.serviceMonitor.enabled`.

## NetworkPolicy

Off by default — the correct policy needs your namespaces and dependency
addresses. With `networkPolicy.enabled`, ingress is default-deny except web →
server and `ingressFrom`. `networkPolicy.egress.enabled` adds egress: DNS,
the rules you list in `egress.to` (Postgres, S3, issuer, SMTP, OTLP), and,
with `egress.allowPublicHttps`, TCP 443 to public addresses only — push
relays and webhook sinks are arbitrary hosts, so RFC 1918, CGNAT, loopback and
link-local (the `169.254.169.254` metadata endpoint) are excepted. The web
pods get no egress at all. `examples/values-production.yaml` shows all of it.

## Secrets

This chart creates no Secret carrying a real credential — every field under
`*.existingSecret` is a reference to one it expects to exist already, made by
whatever your cluster uses for that job (External Secrets, Sealed Secrets,
SOPS, ...). See `values.yaml`'s own comments for which key each field expects.

A webhook sink (declared through `server.extraConfig`) also needs
`server.sinks.fingerprintKey.existingSecret`: the HMAC key its sink id is
fingerprinted under, at least 32 bytes. The server and the migrate Job refuse
to start on a webhook without it. Rotating it re-ids every webhook — follow the
runbook in `docs/ops/runbooks.mdx` rather than just swapping the Secret.

## Filesystem attachments (ADR-0022)

`server.attachments.kind: filesystem` is single-node only — [ADR-0022's own
accepted debt][adr-0022], not a limitation of this chart. Setting it changes
what the chart deploys, not just what it configures:

- `server.replicaCount` is silently overridden to `1`.
- The server's PodDisruptionBudget and HorizontalPodAutoscaler are skipped —
  both would fight a pinned single replica.
- The Deployment's update strategy becomes `Recreate`, so a rollout never
  briefly runs two pods against the same `ReadWriteOnce` PVC.
- `server.attachments.persistence.enabled: true` provisions that PVC. Bring
  your own volume instead via `server.extraVolumes`/`extraVolumeMounts` and
  leave it `false` — either way, the container mounts it at
  `server.attachments.persistence.path`.

A deployment that outgrows one node switches `kind` back to `s3` — nothing
about the data model changes to make that possible ([ADR-0022][adr-0022]).

[adr-0022]: ../../docs/content/docs/adr/0022-filesystem-attachment-store.mdx

## What this chart does not attempt

- **Postgres, Redis, the object store.** No bundled subchart — see above.
- **`[ha] leader_election = "kubernetes"`.** Named in the design as a config
  option, but `notify-server` only implements the `postgres` advisory-lock
  backend today. There is deliberately no `coordination.k8s.io` Lease RBAC
  here; it belongs in this chart the day the code reads that value.
- **A connection-count HPA target.** The design's "HPA (CPU + jauge de
  connexions SSE)" needs a custom-metrics adapter (Prometheus Adapter, KEDA)
  reading `realtime_connections` off `/metrics` — append entries to
  `server.autoscaling.customMetrics` once one is wired into your cluster.
- **A separate metrics port.** `notify-server` binds one port: `/metrics`
  answers on the same 8080 as the API, and the image `EXPOSE`s only that. The
  Service and ServiceMonitor scrape it there; the server Ingress routes
  `/api/v1` only, which is what keeps the exporter off the internet.

## Development

```bash
task helm:lint       # helm lint
task helm:template   # helm template, against examples/values-production.yaml
task helm:package    # package into target/helm/*.tgz (not committed)
task audit:helm      # trivy config, chart-scoped
```
