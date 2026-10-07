/** OIDC discovery, so mock, Authentik and Dex stay interchangeable (ADR-0014, §3.2). */

import { oidcIssuer } from "@/config";

export interface ProviderMetadata {
  issuer: string;
  authorization_endpoint: string;
  token_endpoint: string;
  end_session_endpoint?: string;
  code_challenge_methods_supported?: string[];
}

let cached: Promise<ProviderMetadata> | null = null;

/** Cached once per page load; failures are not cached. */
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
  // OIDC Discovery §4.3: the document must name its issuer (ours has no trailing slash, Authentik's does).
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
  // All endpoints on the issuer's origin, as the CSP `connect-src` assumes.
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
