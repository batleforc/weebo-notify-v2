/**
 * Deployment config from `window.__ENV__`, written by `/env.js` at container boot
 * (§11, ADR-0021); `web/public/env.js` holds the dev values. No secrets: the OIDC
 * client is public (PKCE, §5.1).
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

export const oidcClientId = required("oidcClientId", env.oidcClientId);

/** `groups` drives channel access (§14.2); `offline_access` yields a refresh token. */
export const oidcScopes = "openid profile email groups offline_access";
