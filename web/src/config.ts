/**
 * Everything the frontend needs to know about the deployment it is talking to.
 *
 * Read from `window.__ENV__`, set by a tiny `<script src="/env.js">` `index.html`
 * loads before the app bundle (design §11, ADR-0021). One image now serves every
 * deployment: `web/nginx/docker-entrypoint.d/05-runtime-config.sh` renders
 * `env.js` from `WNK_API_BASE_URL`/`WNK_OIDC_ISSUER`/`WNK_OIDC_CLIENT_ID` at
 * container boot, so starting the image with different env vars is what a
 * different deployment means now — not rebuilding it. `web/public/env.js` is the
 * same file, committed with the local dev stack's values (§12.1), for `pnpm dev`
 * — edit it directly for a different local backend, the same way `config/dev.toml`
 * is edited rather than copied from an example.
 *
 * ADR-0019 chose the build-time form for M1 on the grounds that a runtime
 * `/config.json` was "only worth its extra round trip once the same bundle has to
 * serve several installations" — ADR-0021 is that installation.
 *
 * There is no secret here and there cannot be one: the OIDC client is public
 * (Authorization Code + PKCE, design §5.1), which is precisely why it has no
 * client secret to leak into a bundle or a running container's environment.
 */

/** What `env.js` sets on `window` before this module ever runs. */
export interface RuntimeEnv {
  apiBaseUrl: string;
  oidcIssuer: string;
  oidcClientId: string;
}

declare global {
  interface Window {
    __ENV__?: RuntimeEnv;
  }
}

function required(name: keyof RuntimeEnv, value: string | undefined): string {
  if (!value) {
    throw new Error(
      `Missing window.__ENV__.${name}. Check web/public/env.js for local dev, ` +
        "or the deployment's WNK_* environment variables.",
    );
  }
  return value;
}

const env = window.__ENV__ ?? ({} as Partial<RuntimeEnv>);

/** Base URL of the weebo-notify API, without a trailing slash. */
export const apiBaseUrl = required("apiBaseUrl", env.apiBaseUrl).replace(
  /\/+$/,
  "",
);

/** Issuer of the identity provider — its discovery document hangs off it. */
export const oidcIssuer = required("oidcIssuer", env.oidcIssuer).replace(
  /\/+$/,
  "",
);

/** The public client id registered for this SPA. */
export const oidcClientId = required("oidcClientId", env.oidcClientId);

/**
 * Scopes asked for at login.
 *
 * `groups` is not decoration: channel access is derived from the token's groups
 * claim on every request rather than materialised server-side (§14.2), so a token
 * minted without it reaches only the individual bindings. `offline_access` is what
 * makes a refresh token come back, and therefore what makes a reload survive
 * without a second trip through the provider.
 */
export const oidcScopes = "openid profile email groups offline_access";
