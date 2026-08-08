// Read by `src/config.ts` at startup, before the app bundle runs (design §11,
// ADR-0021). Committed with the values the local dev stack serves — `task dev:run`
// and `task dev:oidc` need no change to work with them — the same way
// `config/dev.toml` is committed rather than copied from an example.
//
// A real deployment does not edit this file: its image renders one just like it,
// at container boot, from WNK_API_BASE_URL/WNK_OIDC_ISSUER/WNK_OIDC_CLIENT_ID/
// WNK_ATTACHMENT_ORIGIN (`web/nginx/docker-entrypoint.d/05-runtime-config.sh`).
// This one only ever ships inside a `pnpm dev`/`pnpm build && pnpm preview` run.
//
// There is no secret in this file, by construction: the OIDC client is public and
// uses Authorization Code + PKCE (§5.1).
window.__ENV__ = {
  apiBaseUrl: "http://localhost:8080",
  oidcIssuer: "http://localhost:9110",
  oidcClientId: "weebo-notify",
};
