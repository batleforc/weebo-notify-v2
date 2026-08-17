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

## Secrets

This chart creates no Secret carrying a real credential — every field under
`*.existingSecret` is a reference to one it expects to exist already, made by
whatever your cluster uses for that job (External Secrets, Sealed Secrets,
SOPS, ...). See `values.yaml`'s own comments for which key each field expects.

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
- **A pre-upgrade migration Job.** `notify-server` runs `sqlx` migrations as
  part of normal boot, guarded by Postgres's own advisory lock, so concurrent
  replicas migrating is already safe. The binary has no `migrate`-and-exit
  mode to hook a Helm pre-upgrade Job to — this chart relies on migrate-on-boot
  plus `/readyz` gating instead of pretending otherwise with a Job that would
  really just run the whole server.
- **A connection-count HPA target.** The design's "HPA (CPU + jauge de
  connexions SSE)" needs a custom-metrics adapter (Prometheus Adapter, KEDA)
  reading `sse_connections` off `/metrics` — append entries to
  `server.autoscaling.customMetrics` once one is wired into your cluster.
- **A separate metrics port.** The server's own `Containerfile` `EXPOSE`s
  8080 and 9090 and describes 9090 as a dedicated Prometheus listener, but
  `notify-server` binds only one port: `/metrics` answers on the same 8080 as
  the API. This chart's Service and ServiceMonitor scrape `/metrics` on that
  one port; the `EXPOSE 9090` is presently aspirational, not a chart gap.

## Development

```bash
task helm:lint       # helm lint
task helm:template   # helm template, against examples/values-production.yaml
task helm:package    # package into target/helm/*.tgz (not committed)
task audit:helm      # trivy config, chart-scoped
```
