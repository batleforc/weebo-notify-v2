/**
 * Authorization Code + PKCE, the parts that are pure functions (design §5.1).
 *
 * PKCE (RFC 7636) is what makes a public client safe without a secret: the app
 * keeps a random `code_verifier`, sends only its SHA-256 hash to the provider, and
 * presents the verifier when redeeming the code. An attacker who intercepts the
 * redirect gets a code they cannot exchange.
 *
 * Everything here uses the Web Crypto API — no dependency, and no way to
 * accidentally end up with `Math.random()` where an unguessable value was meant.
 */

/** Base64url without padding, the encoding OAuth asks for. */
export function base64url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/** `length` bytes of CSPRNG output, base64url encoded. */
export function randomToken(length = 32): string {
  const bytes = new Uint8Array(length);
  crypto.getRandomValues(bytes);
  return base64url(bytes);
}

/**
 * The `code_challenge` for a verifier, using S256.
 *
 * The `plain` method exists in the RFC and is never used here: it puts the
 * verifier itself in a URL that ends up in browser history and proxy logs, which
 * is the attack PKCE was written to close.
 */
export async function challengeOf(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(verifier),
  );
  return base64url(new Uint8Array(digest));
}

/** What a login attempt has to remember while the browser is away at the IdP. */
export interface PendingAuthorization {
  /** The PKCE secret, redeemed with the code. */
  verifier: string;
  /** Anti-CSRF value echoed back by the provider (RFC 6749 §10.12). */
  state: string;
  /** Where to go once the session is open, so a permalink survives the login. */
  returnTo: string;
}

/** Everything a fresh login needs, generated in one place. */
export async function beginAuthorization(returnTo: string): Promise<{
  pending: PendingAuthorization;
  challenge: string;
}> {
  const pending: PendingAuthorization = {
    // 32 bytes → 43 base64url characters, inside the RFC 7636 range of 43–128.
    verifier: randomToken(32),
    state: randomToken(16),
    returnTo,
  };
  return { pending, challenge: await challengeOf(pending.verifier) };
}

/**
 * The claims the frontend reads out of an id token.
 *
 * Read, not trusted: the token this comes from was minted for the server, and the
 * server re-validates it against JWKS on every single request (§5.1). What is
 * decoded here decides what to draw — a display name, a group-gated menu item —
 * never what is allowed. An authorisation decision made in a browser is a
 * suggestion.
 */
export interface IdentityClaims {
  sub: string;
  name?: string;
  email?: string;
  groups?: string[];
}

/** Decode a JWT's payload. Returns `null` for anything that is not one. */
export function readClaims(token: string): IdentityClaims | null {
  const segments = token.split(".");
  if (segments.length !== 3) {
    return null;
  }
  try {
    const payload = segments[1].replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(payload.padEnd(Math.ceil(payload.length / 4) * 4, "="));
    // Through bytes rather than straight to a string: a display name with an
    // accent in it is not exotic, and `atob` alone would mangle it.
    const bytes = Uint8Array.from(binary, (character) =>
      character.charCodeAt(0),
    );
    const claims = JSON.parse(new TextDecoder().decode(bytes));
    return typeof claims?.sub === "string" ? (claims as IdentityClaims) : null;
  } catch {
    return null;
  }
}
