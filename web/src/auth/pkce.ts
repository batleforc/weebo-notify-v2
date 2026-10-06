/** Authorization Code + PKCE (RFC 7636, §5.1), pure functions on Web Crypto. */

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

/** S256 only: `plain` would put the verifier in URLs and logs. */
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

/** Claims read for display only; the server validates the token on every request (§5.1). */
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
