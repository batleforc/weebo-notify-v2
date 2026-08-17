/**
 * The desktop leg of the login (RFC 8252, design §10bis.2).
 *
 * A browser signs in by navigating: the page leaves for the provider and the
 * provider sends it back to `${location.origin}/auth/callback`. The packaged shell
 * can do neither — its origin is `tauri://localhost`, which no identity provider
 * registers, and its window is the app rather than a tab to spend on somebody
 * else's login page. So the shell opens the *system* browser and listens on
 * loopback for the redirect, and this module is the seam between the two.
 *
 * Everything else stays shared: the PKCE verifier, the `state` value and the token
 * exchange all live in `session.ts` exactly as they do in a browser, because the
 * only thing that differs between the two worlds is how the code gets back.
 */

/**
 * Where the provider sends the code on desktop.
 *
 * Registered verbatim in `dev/oidc-mock/identities.toml`, and covered by the
 * `http://127.0.0.1:[0-9]+/auth/callback` pattern the Authentik blueprint uses.
 * The port is spelled out again in `web/src-tauri/src/oauth.rs`, which explains
 * why it is fixed rather than ephemeral; the two are only ever right together.
 */
export const LOOPBACK_REDIRECT_URI = "http://127.0.0.1:1420/auth/callback";

/**
 * Open `authorizeUrl` in the system browser and resolve with the query string the
 * provider redirects back with.
 *
 * `@tauri-apps/api` is imported dynamically for the same reason `deeplink.ts`
 * does it: the browser build must not carry a desktop package, and a static
 * import would put one in every bundle whether or not the branch is ever taken.
 */
export async function awaitLoopbackCallback(
  authorizeUrl: string,
): Promise<URLSearchParams> {
  const { invoke } = await import("@tauri-apps/api/core");
  // `authorizeUrl` here, `authorize_url` there: Tauri maps command arguments
  // between the two conventions.
  const query = await invoke<string>("loopback_authorize", { authorizeUrl });
  return new URLSearchParams(query);
}

/**
 * Open a URL in the system browser rather than in the shell's own window.
 *
 * For logout: the provider's session cookie belongs to the browser that signed
 * in, so that is where an RP-initiated logout has to land.
 */
export async function openExternal(url: string): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("open_external", { url });
}
