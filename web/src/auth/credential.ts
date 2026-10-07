/**
 * Desktop refresh-token storage in the OS vault (§10bis.2), which also lets the
 * shell start in the tray with a session. Tauri is imported dynamically to keep it
 * out of the browser bundle. The access token stays in memory.
 */

/** Hand the credential to the OS vault, or to the `0600` fallback (§9.3bis). */
export async function saveCredential(secret: string): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("credential_save", { secret });
}

/** `null` on first launch is normal, not an error. */
export async function loadCredential(): Promise<string | null> {
  const { invoke } = await import("@tauri-apps/api/core");
  return (await invoke<string | null>("credential_load")) ?? null;
}

/** Forget it, in both stores. Sign-out, and anything that invalidates a session. */
export async function clearCredential(): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("credential_clear");
}

/**
 * Hand the access token and display name to the Rust tray/poll loop, which would
 * otherwise only see `notify login`'s token. `null`/`null` on sign-out.
 */
export async function shareSessionToken(
  token: string | null,
  name: string | null,
): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("credential_session", { token, name });
}
