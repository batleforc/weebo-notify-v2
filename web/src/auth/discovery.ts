/**
 * OIDC discovery: ask the provider where its endpoints are instead of hard-coding
 * three more URLs per deployment.
 *
 * This is also what keeps the mock (ADR-0014), Authentik and Dex interchangeable
 * from the frontend's point of view — the whole point of the `oidc-compat` matrix
 * (§3.2) is that our own assumptions are the risk, and an assumption about a URL
 * shape is exactly the kind that hides until the day it does not.
 */

import { oidcIssuer } from "@/config";

export interface ProviderMetadata {
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
  end_session_endpoint?: string;
  code_challenge_methods_supported?: string[];
}

let cached: Promise<ProviderMetadata> | null = null;

/**
 * Fetch (once per page load) the provider's discovery document.
 *
 * Only a success is kept: a failure — offline at boot, the provider restarting —
 * is forgotten so the next call asks again instead of failing until a reload.
 */
export function discover(): Promise<ProviderMetadata> {
  if (!cached) {
    const attempt = load();
    cached = attempt;
    attempt.catch(() => {
      if (cached === attempt) {
        cached = null;
      }
    });
  }
  return cached;
}

async function load(): Promise<ProviderMetadata> {
  const url = `${oidcIssuer}/.well-known/openid-configuration`;
  const response = await fetch(url, {
    headers: { accept: "application/json" },
  });
  if (!response.ok) {
    throw new Error(
      `The identity provider at ${oidcIssuer} did not answer discovery (${response.status}).`,
    );
  }

  const metadata = (await response.json()) as ProviderMetadata;
  // OIDC Discovery §4.3: the document must name the issuer it was fetched from.
  // Anything else is a document we did not ask for, and its endpoints — the
  // logout one is opened by the desktop shell — are not ours to trust.
  // `oidcIssuer` is stored without a trailing slash; Authentik's has one.
  if (metadata.issuer?.replace(/\/+$/, "") !== oidcIssuer) {
    throw new Error(
      `The discovery document at ${url} names another issuer (${metadata.issuer}).`,
    );
  }
  if (!metadata.authorization_endpoint || !metadata.token_endpoint) {
    throw new Error(
      `The discovery document at ${url} is missing an endpoint we need.`,
    );
  }
  // Every endpoint on the issuer's own origin — which the CSP's `connect-src`
  // already assumes for the token endpoint (`lib/csp.ts`), and which keeps the
  // logout URL the desktop shell opens in the system browser a page we chose.
  const origin = new URL(oidcIssuer).origin;
  for (const endpoint of [
    metadata.authorization_endpoint,
    metadata.token_endpoint,
    metadata.end_session_endpoint,
  ]) {
    if (endpoint && new URL(endpoint).origin !== origin) {
      throw new Error(
        `The discovery document at ${url} points outside ${origin} (${endpoint}).`,
      );
    }
  }

  // Refusing here rather than starting a flow that cannot be secure: a provider
  // without S256 would leave the verifier in a URL (see `pkce.ts`).
  const methods = metadata.code_challenge_methods_supported;
  if (methods && !methods.includes("S256")) {
    throw new Error(
      `The identity provider does not support PKCE with S256 (advertises: ${methods.join(", ")}).`,
    );
  }

  return metadata;
}

/** Drop the cached document — used by tests, and by nothing else. */
export function forgetDiscovery(): void {
  cached = null;
}
