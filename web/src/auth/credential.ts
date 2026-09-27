/**
 * The desktop half of "where the refresh token lives" (design §10bis.2).
 *
 * A browser keeps it in `sessionStorage` and [ADR-0020] explains at length why that
 * is the right answer *there*: the exposure is an XSS on this origin, the CSP has
 * narrowed the way in and out, and rotation makes a stolen token single-use. None
 * of that reasoning transfers to the shell, which §10bis.2 puts under a different
 * rule — "tokens stockés dans le coffre de l'OS […] jamais en localStorage ni dans
 * la webview".
 *
 * It is also the difference between a shell that can start in the tray and one that
 * cannot. `sessionStorage` dies with the process, so before this module every launch
 * arrived with no session and had to show a login screen; §10bis.3's "démarre alors
 * directement en tray, fenêtre cachée" had nothing to start from.
 *
 * `@tauri-apps/api` is imported dynamically for the same reason `deeplink.ts` and
 * `loopback.ts` do it: the browser build must not carry a desktop package, and a
 * static import would put one in every bundle whether or not the branch is taken.
 *
 * Only the *refresh* token comes through here. The access token stays in memory
 * where it already was: it is the short-lived half, and persisting it would widen
 * what is on disk without shortening any path back in.
 */

/** Hand the credential to the OS vault, or to the `0600` fallback (§9.3bis). */
export async function saveCredential(secret: string): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("credential_save", { secret });
}

/**
 * Read it back, or `null` on a machine that holds none.
 *
 * `null` is the ordinary first-launch answer and is not an error: the Rust side
 * separates "no entry" from "a vault that is failing", and only the second is worth
 * anyone's attention.
 */
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
 * Hand the current **access** token and who it belongs to down to the Rust half,
 * or `null`/`null` on sign-out.
 *
 * The name is for the tray's header line — `weebo-notify — Alice Martin` — and for
 * nothing else. It travels with the token rather than on a wire of its own because
 * it answers the same question at the same moment: this is who signed in, and this
 * is what to run as. Two calls would let those two disagree.
 *
 * Nothing to do with the vault above, and deliberately in this module anyway: both
 * are "the credential leaving the webview", and splitting them would put two halves
 * of one question in two files.
 *
 * The Rust half — tray, poll loop, store-warming daemon — reads `Profile.token`,
 * which only `notify login` ever writes. So signing in through the window used to
 * leave the tray on `auth.invalid_credential` for the whole run, no matter how
 * healthy the window was. This is the wire that fixes it.
 *
 * The access token and not the refresh one: it is what a `Bearer` header carries,
 * and `Profile.token`'s own documentation allows exactly this ("an OIDC access
 * token or an API key"). It expires in fifteen minutes and is re-sent on every
 * rotation, which is why the Rust side keeps it in memory and never writes it to
 * `config.toml` — a CLI that inherited it would work until lunchtime.
 */
export async function shareSessionToken(
  token: string | null,
  name: string | null,
): Promise<void> {
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("credential_session", { token, name });
}
