/**
 * Desktop login over loopback (RFC 8252, §10bis.2): no IdP registers `tauri://`,
 * so the system browser signs in and the code comes back on 127.0.0.1.
 */

/** Registered in `dev/oidc-mock/identities.toml`; port fixed in `web/src-tauri/src/oauth.rs`. */
export const LOOPBACK_REDIRECT_URI = "http://127.0.0.1:1420/auth/callback";

/** Open the system browser and resolve with the redirect's query string. */
export async function awaitLoopbackCallback(
  authorizeUrl: string,
): Promise<URLSearchParams> {
  const { invoke } = await import("@tauri-apps/api/core");
  const query = await invoke<string>("loopback_authorize", { authorizeUrl });
  return new URLSearchParams(query);
}

/** For logout: the IdP cookie lives in the system browser. */
export async function openExternal(url: string): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("open_external", { url });
}
