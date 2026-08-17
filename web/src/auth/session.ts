/**
 * The session: one access token, kept fresh, handed to whoever calls the API.
 *
 * # Where the tokens live, and what that costs
 *
 * Design §5.1 asks for tokens "out of localStorage — memory plus a refresh driven
 * by the service worker". Half of that ships here: the **access token never leaves
 * memory**, so no XSS-readable storage ever holds a credential that opens the API.
 *
 * The other half — "a refresh driven by the service worker" — **is not coming**,
 * and this comment used to say it was. The worker exists since M3 and the move it
 * was supposed to make buys nothing: a worker is killed aggressively, so to survive
 * eviction the token would have to live in IndexedDB, which the page reads exactly
 * as well as the worker does. Keeping it in worker memory instead means a trip
 * through the identity provider on every eviction, which is the "everything in
 * memory" option §5.1's own reasoning rejected.
 *
 * So the refresh token stays in `sessionStorage`, now as a **decision rather than
 * an interim** (ADR-0020, which supersedes ADR-0016 for exactly this reason). The
 * trade, stated:
 *
 *   * `sessionStorage` is scoped to the tab and cleared when it closes, so a
 *     shared machine does not keep a session alive behind the person who left;
 *   * without it, every reload would bounce through the identity provider — a
 *     redirect on every F5, which is how people learn to leave a tab open forever
 *     instead;
 *   * the refresh token rotates on every use (§5.1), so a stolen one is single-use
 *     and its reuse is detectable by the provider.
 *
 * The exposure this leaves is an XSS able to read `sessionStorage`. The same XSS
 * could call the API directly with the in-memory token, so what it adds is
 * **duration**, not access — hours instead of the fifteen minutes an access token
 * lives.
 *
 * That is an accepted risk, and it rests on three things holding: the bundle's
 * strict CSP (ADR-0019 — `script-src` without `'unsafe-inline'`, `connect-src`
 * naming only the API and the issuer), the three-gate markdown path (`lib/markdown`),
 * and refresh rotation with reuse detection at the provider. **Weakening any of
 * those reopens the decision**, and so does a deployment whose channels carry
 * something worse to disclose than the API access an XSS already has.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { oidcClientId, oidcScopes } from "@/config";
import { isDesktop } from "@/lib/desktop";
import { discover } from "./discovery";
import {
  awaitLoopbackCallback,
  LOOPBACK_REDIRECT_URI,
  openExternal,
} from "./loopback";
import {
  beginAuthorization,
  type IdentityClaims,
  type PendingAuthorization,
  readClaims,
} from "./pkce";

/**
 * Where the provider sends the browser back. Registered in the IdP (§12.5).
 *
 * The origin is the right answer in a browser and the wrong one in the shell,
 * where it is `tauri://localhost` — a scheme no provider will register. Desktop
 * therefore uses the loopback redirect of RFC 8252 (see `./loopback`). Read once
 * at module load, which is safe in both: Tauri injects its globals before the app
 * bundle runs, and a browser never grows them.
 */
export const redirectUri = isDesktop()
  ? LOOPBACK_REDIRECT_URI
  : `${window.location.origin}/auth/callback`;

const PENDING_KEY = "weebo-notify:pending-auth";
const REFRESH_KEY = "weebo-notify:refresh-token";

/** Refresh this long before expiry, so no in-flight request rides a dead token. */
const REFRESH_MARGIN_MS = 60_000;

interface TokenResponse {
  access_token: string;
  refresh_token?: string;
  id_token?: string;
  expires_in?: number;
  token_type: string;
}

export const useSession = defineStore("session", () => {
  const accessToken = ref<string | null>(null);
  const expiresAt = ref<number>(0);
  const claims = ref<IdentityClaims | null>(null);
  /** Set when a refresh failed for good: the UI shows "signed out", not a spinner. */
  const expired = ref(false);

  /** One in-flight refresh at most — twenty concurrent 401s must not mint twenty. */
  let refreshing: Promise<string | null> | null = null;

  const authenticated = computed(() => accessToken.value !== null);
  const displayName = computed(
    () => claims.value?.name ?? claims.value?.email ?? claims.value?.sub ?? "",
  );
  /** Groups as the token carries them — used to show, never to allow (§14.2). */
  const groups = computed(() => claims.value?.groups ?? []);

  function storedRefreshToken(): string | null {
    return sessionStorage.getItem(REFRESH_KEY);
  }

  function remember(tokens: TokenResponse): void {
    accessToken.value = tokens.access_token;
    expiresAt.value = Date.now() + (tokens.expires_in ?? 900) * 1000;
    expired.value = false;
    if (tokens.id_token) {
      claims.value = readClaims(tokens.id_token);
    }
    if (tokens.refresh_token) {
      sessionStorage.setItem(REFRESH_KEY, tokens.refresh_token);
    }
  }

  function forget(): void {
    accessToken.value = null;
    expiresAt.value = 0;
    claims.value = null;
    sessionStorage.removeItem(REFRESH_KEY);
    sessionStorage.removeItem(PENDING_KEY);
  }

  /**
   * Send the person to the provider.
   *
   * Returns `null` in a browser, where this never really returns at all — the page
   * navigates away and the answer comes back through `/auth/callback`. On desktop
   * it returns where to go next, because the window never left: the code arrives
   * over loopback and is redeemed before this resolves.
   */
  async function login(returnTo = "/"): Promise<string | null> {
    const metadata = await discover();
    const { pending, challenge } = await beginAuthorization(returnTo);
    sessionStorage.setItem(PENDING_KEY, JSON.stringify(pending));

    const url = new URL(metadata.authorization_endpoint);
    url.searchParams.set("response_type", "code");
    url.searchParams.set("client_id", oidcClientId);
    url.searchParams.set("redirect_uri", redirectUri);
    url.searchParams.set("scope", oidcScopes);
    url.searchParams.set("state", pending.state);
    url.searchParams.set("code_challenge", challenge);
    url.searchParams.set("code_challenge_method", "S256");

    // The desktop branch redeems the code here rather than in `CallbackView`,
    // which is a route the shell never visits: nothing navigates, so there is no
    // callback page to land on. `completeLogin` is shared all the same — the
    // `state` check and the token exchange are the parts that must not have two
    // implementations.
    if (isDesktop()) {
      return completeLogin(await awaitLoopbackCallback(url.toString()));
    }

    window.location.assign(url.toString());
    return null;
  }

  /**
   * Redeem the code the provider sent back.
   *
   * Returns where to navigate next — the permalink the person opened before being
   * bounced through the login, so `/n/<id>` in a CI log survives a cold browser.
   */
  async function completeLogin(params: URLSearchParams): Promise<string> {
    const raw = sessionStorage.getItem(PENDING_KEY);
    sessionStorage.removeItem(PENDING_KEY);
    if (!raw) {
      throw new Error("No login was started from this tab.");
    }
    const pending: PendingAuthorization = JSON.parse(raw);

    const error = params.get("error");
    if (error) {
      throw new Error(params.get("error_description") ?? error);
    }

    // The state check is the CSRF gate: a code delivered to this redirect URI by
    // anything other than the flow this tab started is refused (RFC 6749 §10.12).
    if (params.get("state") !== pending.state) {
      throw new Error("The provider answered a login this tab did not start.");
    }

    const code = params.get("code");
    if (!code) {
      throw new Error("The provider came back without an authorization code.");
    }

    const metadata = await discover();
    const body = new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: redirectUri,
      client_id: oidcClientId,
      code_verifier: pending.verifier,
    });

    remember(await redeem(metadata.token_endpoint, body));
    return pending.returnTo || "/";
  }

  /**
   * Trade the refresh token for a new access token.
   *
   * `null` means the session is over and the caller should send the person back
   * to the provider — an expired refresh token is the normal end of a session,
   * not an error worth a dialog.
   */
  async function refresh(): Promise<string | null> {
    if (refreshing) {
      return refreshing;
    }

    const token = storedRefreshToken();
    if (!token) {
      forget();
      expired.value = true;
      return null;
    }

    refreshing = (async () => {
      try {
        const metadata = await discover();
        const tokens = await redeem(
          metadata.token_endpoint,
          new URLSearchParams({
            grant_type: "refresh_token",
            refresh_token: token,
            client_id: oidcClientId,
          }),
        );
        remember(tokens);
        return tokens.access_token;
      } catch {
        // Rotation means a refresh token is single-use: a failure here is a
        // session that has ended, whatever the reason.
        forget();
        expired.value = true;
        return null;
      } finally {
        refreshing = null;
      }
    })();

    return refreshing;
  }

  /**
   * The token to put on the next request, refreshing first if it is about to die.
   *
   * Every API call goes through this rather than reading `accessToken` directly:
   * a fifteen-minute token (§12.5) expires mid-session by design, and the only
   * place that should ever notice is here.
   */
  async function bearer(): Promise<string | null> {
    if (!accessToken.value) {
      return storedRefreshToken() ? refresh() : null;
    }
    if (Date.now() >= expiresAt.value - REFRESH_MARGIN_MS) {
      return refresh();
    }
    return accessToken.value;
  }

  /**
   * Restore a session across a reload, using the refresh token from this tab.
   *
   * Returns whether there is a session to work with, so the router can decide
   * between the app and the login screen without a flash of the wrong one.
   */
  async function restore(): Promise<boolean> {
    if (accessToken.value) {
      return true;
    }
    return storedRefreshToken() ? (await refresh()) !== null : false;
  }

  /** Sign out here, and at the provider when it offers a way to. */
  async function logout(): Promise<void> {
    forget();
    const metadata = await discover().catch(() => null);
    if (metadata?.end_session_endpoint) {
      const url = new URL(metadata.end_session_endpoint);
      url.searchParams.set("client_id", oidcClientId);

      // The shell must not follow this one itself. The provider's session cookie
      // belongs to the browser that signed in (see `./loopback`), so ending it
      // there is the only thing that ends it — and navigating the app's own
      // window to a logout page would strand it on the provider's site with the
      // local session already forgotten. No `post_logout_redirect_uri` either:
      // `tauri://localhost/` is not registered, and there is nothing to come back
      // to, since the app never left.
      if (isDesktop()) {
        await openExternal(url.toString()).catch(() => {
          // A logout that cannot reach the browser has still forgotten
          // everything this app held, which is the half that protects this
          // machine. Nothing is worth an error dialog here.
        });
      } else {
        url.searchParams.set(
          "post_logout_redirect_uri",
          `${window.location.origin}/`,
        );
        window.location.assign(url.toString());
        return;
      }
    }

    // Both roads end here on desktop, and the browser's no-`end_session_endpoint`
    // road too: reload the app at its root. The store is already empty, so the
    // router's guard lands on the login screen — and going through a reload rather
    // than a `router.push` is what guarantees no component is still holding a
    // rendered copy of what the previous session was reading.
    window.location.assign("/");
  }

  return {
    accessToken,
    authenticated,
    claims,
    displayName,
    expired,
    expiresAt,
    groups,
    bearer,
    completeLogin,
    login,
    logout,
    refresh,
    restore,
  };
});

async function redeem(
  endpoint: string,
  body: URLSearchParams,
): Promise<TokenResponse> {
  const response = await fetch(endpoint, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body,
  });

  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    throw new Error(
      `The identity provider refused the token request (${response.status}). ${detail}`.trim(),
    );
  }

  return (await response.json()) as TokenResponse;
}
