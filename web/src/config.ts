/**
 * Everything the frontend needs to know about the deployment it is talking to.
 *
 * Read from Vite's `import.meta.env`, which inlines the values at build time — so
 * a deployment is a build, not a runtime file. That is deliberate for M1: the
 * alternative (a `/config.json` fetched at boot) is only worth its extra round
 * trip once the same bundle has to serve several installations, which is a
 * question for whoever packages the container image (§11), not for the SPA.
 *
 * There is no secret here and there cannot be one: the OIDC client is public
 * (Authorization Code + PKCE, design §5.1), which is precisely why it has no
 * client secret to leak into a bundle.
 */

function required(name: string, value: string | undefined): string {
  if (!value) {
    throw new Error(
      `Missing ${name}. Copy web/.env.example to web/.env.local and fill it in.`,
    );
  }
  return value;
}

/** Base URL of the weebo-notify API, without a trailing slash. */
export const apiBaseUrl = required(
  "VITE_API_BASE_URL",
  import.meta.env.VITE_API_BASE_URL,
).replace(/\/+$/, "");

/** Issuer of the identity provider — its discovery document hangs off it. */
export const oidcIssuer = required(
  "VITE_OIDC_ISSUER",
  import.meta.env.VITE_OIDC_ISSUER,
).replace(/\/+$/, "");

/** The public client id registered for this SPA. */
export const oidcClientId = required(
  "VITE_OIDC_CLIENT_ID",
  import.meta.env.VITE_OIDC_CLIENT_ID,
);

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
