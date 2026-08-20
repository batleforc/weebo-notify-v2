// Read by `src/config.ts` at startup, before the app bundle runs (design §11,
// ADR-0021). WRITTEN BY `task dev:web:env`, which `task dev:web` depends on —
// so edits here last until the next `task dev:web` and no longer. Committed
// with the values the local dev stack serves, which is what that task writes on
// any machine that is not an Eclipse Che workspace (§12.1).
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
