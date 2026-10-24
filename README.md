# weebo-notify

A self-hosted notification server where a notification is a lifecycle object, not a message:
it has an identity, a state and a history of revisions. A CI run, an incident or a long
script is one entry that goes from open to closed, not a stream of lines.

- **Server**: Rust (actix), Postgres, OIDC sign-in, API keys and federated CI tokens.
- **Delivery**: SSE and WebSocket streams, Web Push, UnifiedPush, APNs, email and webhook sinks.
- **Clients**: web app (Vue), desktop and Android (Tauri), CLI and terminal UI.

## Getting started

```bash
task init        # toolchain (mise), git hooks, local secrets
task dev:up      # Postgres + RustFS
task dev:oidc    # development identity provider on :9110 (alice / weebo-dev)
task dev:run     # server on :8080
task dev:web     # web app on :5173
```

`task --list` shows every command. The documentation (architecture, configuration,
clients, API, decisions) is in [`docs/`](docs/content/docs) and runs with `task dev:docs`.

## Status

Feature-complete for v1; work now goes into stability. Open items are in [`TODO.md`](TODO.md).

## License

[Apache-2.0](LICENSE.md)
