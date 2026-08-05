# weebo-notify

Rich, multi-channel, multi-device notification platform. Replaces Gotify and ntfy in
the `weebo-si` stack.

The difference is the core object: **a notification is a lifecycle, not a message.**
It has an identity, a state, and a history of revisions. A CI pipeline pushes
`state=open` when it starts and `state=closed, status=success` when it ends — under
the same id. One inbox entry that went from running to succeeded, not two lines of
noise.

## Getting started

```bash
task init        # toolchain (mise) + git hooks (cocogitto) + the local secrets
task dev:up      # Postgres + RustFS, ~20 s
task dev:seed    # migrations, attachment bucket, test identities
task dev:oidc    # the development identity provider, on :9110
task dev:run     # the server, on :8080
```

Log in as `alice`, `bob` or `carol`, password `weebo-dev`.

`task --list` is the complete command inventory. The
[daily workflow](docs/content/docs/dev/workflow.mdx) explains what each one does.

## Layout

| Path | What lives there |
| --- | --- |
| `crates/notify-domain` | Entities, invariants, the state machine. Zero I/O. |
| `crates/notify-ports` | Async traits, plus in-memory doubles for testing |
| `crates/notify-app` | Use cases |
| `crates/notify-adapters` | HTTP, Postgres, event bus, OIDC, object store, push transports |
| `crates/notify-server` | Composition root — one binary |
| `crates/notify-cli` | The `notify` binary: CLI, and the TUI from M2 |
| `crates/oidc-mock` | The development identity provider — never published, never deployed |
| `crates/xtask` | Repository chores that need real code (client codegen, coverage gate) |
| `web/` | Vue 3 frontend (from M1) |
| `docs/` | The documentation site (Fumadocs) |
| `dev/` | The development stack: Compose, seeds, test identities |

## Documentation

```bash
task dev:docs    # http://localhost:3000
```

- [Development environment](docs/content/docs/dev/environment.mdx) — one command, identical under Compose and Eclipse Che
- [Architecture](docs/content/docs/architecture/index.mdx) — hexagonal layers and the rules that hold them apart
- [Configuration](docs/content/docs/ops/configuration.mdx) — layered TOML and `WNK_` environment
- [Hardening](docs/content/docs/ops/hardening.mdx) — what the deployment must provide, because the application deliberately does not
- [Decisions](docs/content/docs/adr/index.mdx) — why it is built this way, including the debts taken on knowingly

The full design specification lives in
[`docs/.prompt/weebo-notify-design.md`](docs/.prompt/weebo-notify-design.md).

## Contributing

Commits follow [Conventional Commits](https://www.conventionalcommits.org/), verified
by the cocogitto `commit-msg` hook. The `pre-commit` hook runs `task recu`,
`task lint`, gitleaks and the whitespace fixers — so drift is normally caught before
it reaches CI.

Every generated artefact — the OpenAPI document, both derived clients, the sqlx
offline data — is committed, and the CI `contract` job regenerates and diffs them. A
client can never disagree with the server it talks to.

Bootstrapped from [`weebo-base`](https://github.com/batleforc/weebo-base).
