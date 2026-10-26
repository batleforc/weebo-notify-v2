/**
 * The session: the access token lives in memory only (§5.1). The refresh token
 * sits in `sessionStorage` in a browser (ADR-0020: accepted risk, relies on the
 * strict CSP, the markdown gates and refresh rotation) and in the OS vault on
 * desktop (§10bis.2, via `./credential`).
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { oidcClientId, oidcScopes } from "@/config";
import { i18n } from "@/i18n";
import { isDesktop } from "@/lib/desktop";
import {
  clearCredential,
  loadCredential,
  saveCredential,
  shareSessionToken,
} from "./credential";
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
import { announcePresence, anotherTabSignedIn } from "./presence";

/** Registered in the IdP (§12.5). Desktop uses RFC 8252 loopback: no IdP registers `tauri://`. */
export const redirectUri = isDesktop()
  ? LOOPBACK_REDIRECT_URI
  : `${window.location.origin}/auth/callback`;

const PENDING_KEY = "weebo-notify:pending-auth";
const REFRESH_KEY = "weebo-notify:refresh-token";

/** How long sign-out waits for the device revocation before going ahead anyway. */
const LOGOUT_PUSH_TIMEOUT_MS = 3_000;

/** Refresh this long before expiry, so no in-flight request rides a dead token. */
const REFRESH_MARGIN_MS = 60_000;

/** The provider answered "no". Only this ends a session; a network failure keeps the refresh token. */
class TokenRefused extends Error {
  constructor(
    readonly status: number,
    detail: string,
  ) {
    super(
      `The identity provider refused the token request (${status}). ${detail}`.trim(),
    );
  }
}

/**
 * "Duplicate tab" copies `sessionStorage`, so two tabs share one single-use refresh
 * token. A rotating tab announces `{ from, to }`; tabs still holding `from` adopt `to`.
 */
const rotations: BroadcastChannel | null =
  typeof BroadcastChannel === "undefined"
    ? null
    : new BroadcastChannel("weebo-notify:refresh-rotation");

interface TokenResponse {
  access_token: string;
  refresh_token?: string;
  id_token?: string;
  expires_in?: number;
  token_type: string;
}

/** RFC 7009, best-effort and bounded like the push revocation: sign-out never waits on the IdP. */
async function revoke(endpoint: string, token: string): Promise<void> {
  try {
    await fetch(endpoint, {
      method: "POST",
      body: new URLSearchParams({
        token,
        token_type_hint: "refresh_token",
        client_id: oidcClientId,
      }),
      signal: AbortSignal.timeout(LOGOUT_PUSH_TIMEOUT_MS),
    }).then((response) => {
      if (!response.ok) {
        console.warn(
          `weebo-notify: the refresh token revocation answered ${response.status}`,
        );
      }
    });
  } catch (error) {
    console.warn("weebo-notify: could not revoke the refresh token", error);
  }
}

export const useSession = defineStore("session", () => {
  const accessToken = ref<string | null>(null);
  const expiresAt = ref<number>(0);
  const claims = ref<IdentityClaims | null>(null);
  /** Set when a refresh failed for good: the UI shows "signed out", not a spinner. */
  const expired = ref(false);

  /** Desktop only: the URL a loopback sign-in waits on, so the view can offer to reopen it. */
  const pendingAuthorizeUrl = ref<string | null>(null);

  /** One in-flight refresh at most — twenty concurrent 401s must not mint twenty. */
  let refreshing: Promise<string | null> | null = null;

  const authenticated = computed(() => accessToken.value !== null);
  const displayName = computed(
    () => claims.value?.name ?? claims.value?.email ?? claims.value?.sub ?? "",
  );
  /** Groups as the token carries them — used to show, never to allow (§14.2). */
  const groups = computed(() => claims.value?.groups ?? []);

  /** Desktop's sync mirror of the async OS vault, hydrated in `restore`. Every write also hits the vault. */
  const refreshToken = ref<string | null>(null);

  // The shell has no browser-wide push to keep alive for anyone (`lib/push` is
  // web-only), so it neither asks nor answers.
  if (!isDesktop()) {
    announcePresence(() => storedRefreshToken() !== null);
  }

  if (rotations && !isDesktop()) {
    rotations.onmessage = (
      event: MessageEvent<{ from: string; to: string }>,
    ) => {
      if (sessionStorage.getItem(REFRESH_KEY) === event.data.from) {
        sessionStorage.setItem(REFRESH_KEY, event.data.to);
      }
    };
  }

  function storedRefreshToken(): string | null {
    return isDesktop()
      ? refreshToken.value
      : sessionStorage.getItem(REFRESH_KEY);
  }

  /** Vault write is not awaited: an unavailable keyring must not block sign-in (§9.3bis). */
  function persistRefreshToken(token: string): void {
    refreshToken.value = token;
    if (!isDesktop()) {
      const previous = sessionStorage.getItem(REFRESH_KEY);
      sessionStorage.setItem(REFRESH_KEY, token);
      if (previous && previous !== token) {
        rotations?.postMessage({ from: previous, to: token });
      }
      return;
    }
    void saveCredential(token).catch((error: unknown) => {
      console.error("weebo-notify: could not persist the credential", error);
    });
  }

  function remember(tokens: TokenResponse): void {
    accessToken.value = tokens.access_token;
    expiresAt.value = Date.now() + (tokens.expires_in ?? 900) * 1000;
    expired.value = false;
    if (tokens.id_token) {
      const read = readClaims(tokens.id_token);
      claims.value = read;
      if (read && !isDesktop()) {
        const subject = read.sub;
        // A device another person enrolled on this browser must not carry on
        // under this one: their notifications would keep ringing here.
        void import("@/lib/push")
          .then((push) => push.claimFor(subject))
          .catch((error: unknown) => {
            console.error(
              "weebo-notify: could not check the push owner",
              error,
            );
          });
      }
    }
    if (tokens.refresh_token) {
      persistRefreshToken(tokens.refresh_token);
    }
    shareWithShell(accessToken.value);
  }

  /** Hand the Rust tray its credential on every rotation; failure only stales the tray. */
  function shareWithShell(token: string | null): void {
    if (!isDesktop()) {
      return;
    }
    const who = token ? displayName.value || null : null;
    void shareSessionToken(token, who).catch((error: unknown) => {
      console.error(
        "weebo-notify: could not hand the token to the shell",
        error,
      );
    });
  }

  function forget(): void {
    accessToken.value = null;
    expiresAt.value = 0;
    claims.value = null;
    refreshToken.value = null;
    sessionStorage.removeItem(REFRESH_KEY);
    sessionStorage.removeItem(PENDING_KEY);
    shareWithShell(null);
    // Drop the browser-wide push subscription only once no other tab is signed in
    // (`./presence`); the push service's `410` then revokes the device server-side.
    if (!isDesktop()) {
      void anotherTabSignedIn()
        .then(async (others) => {
          if (!others) {
            await (await import("@/lib/push")).dropLocalPush();
          }
        })
        .catch((error: unknown) => {
          console.error(
            "weebo-notify: could not drop the push subscription",
            error,
          );
        });
    } else {
      void clearCredential().catch((error: unknown) => {
        console.error("weebo-notify: could not clear the credential", error);
      });
    }
  }

  /** Browser: navigates away, returns `null`. Desktop: redeems the loopback code, returns where to go. */
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

    if (isDesktop()) {
      // Cleared as soon as the loopback port closes, before the code exchange.
      pendingAuthorizeUrl.value = url.toString();
      let callback: URLSearchParams;
      try {
        callback = await awaitLoopbackCallback(url.toString());
      } finally {
        pendingAuthorizeUrl.value = null;
      }
      return completeLogin(callback);
    }

    window.location.assign(url.toString());
    return null;
  }

  /** Redeem the code; returns the permalink the person opened before signing in. */
  async function completeLogin(params: URLSearchParams): Promise<string> {
    const raw = sessionStorage.getItem(PENDING_KEY);
    if (!raw) {
      throw new Error(i18n.global.t("callback.error.noPending"));
    }
    const pending: PendingAuthorization = JSON.parse(raw);

    // CSRF gate (RFC 6749 §10.12), before anything else is believed: a forged
    // callback, `error` included, neither cancels the sign-in in progress nor
    // puts its own text on screen.
    if (params.get("state") !== pending.state) {
      throw new Error(i18n.global.t("callback.error.state"));
    }
    sessionStorage.removeItem(PENDING_KEY);

    const error = params.get("error");
    if (error) {
      throw new Error(params.get("error_description") ?? error);
    }

    const code = params.get("code");
    if (!code) {
      throw new Error(i18n.global.t("callback.error.noCode"));
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

  /** `null` means no token right now; `expired` says whether the session is over. */
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
      } catch (error) {
        // Only an answered 400/401 ends the session, and not if another tab
        // already rotated the token under us.
        if (
          error instanceof TokenRefused &&
          (error.status === 400 || error.status === 401) &&
          storedRefreshToken() === token
        ) {
          forget();
          expired.value = true;
        }
        return null;
      } finally {
        refreshing = null;
      }
    })();

    return refreshing;
  }

  /** The token for the next request, refreshed first if about to expire (§12.5). */
  async function bearer(): Promise<string | null> {
    if (!accessToken.value) {
      return storedRefreshToken() ? refresh() : null;
    }
    if (Date.now() >= expiresAt.value - REFRESH_MARGIN_MS) {
      return refresh();
    }
    return accessToken.value;
  }

  /** Whether a refresh token is held: a `null` from `bearer()` was transient, not the end. */
  function canResume(): boolean {
    return storedRefreshToken() !== null;
  }

  /** Restore across a reload (and a restart on desktop). The only place the vault is read. */
  async function restore(): Promise<boolean> {
    if (accessToken.value) {
      return true;
    }

    if (isDesktop() && refreshToken.value === null) {
      refreshToken.value = await loadCredential().catch((error: unknown) => {
        console.error("weebo-notify: could not read the credential", error);
        return null;
      });
    }

    if (!storedRefreshToken()) {
      return false;
    }
    // Offline boot: the token is kept and the next API call retries.
    return (await refresh()) !== null || storedRefreshToken() !== null;
  }

  /** Sign out here, and at the provider when it offers a way to. */
  async function logout(options?: {
    /** Desktop: run once signed out, before the reload ("Change server"). */
    afterSignOut?: () => Promise<void>;
  }): Promise<void> {
    // Revoke the device while there is still a token to do it with. Best-effort
    // and bounded: a slow or unreachable API must not keep someone signed in.
    await Promise.race([
      import("@/lib/push")
        .then((push) => push.disable())
        .catch(() => undefined),
      new Promise((resolve) => setTimeout(resolve, LOGOUT_PUSH_TIMEOUT_MS)),
    ]);
    // The local half regardless of how the race above ended: a revocation the
    // API never answered must still not leave this browser subscribed. Imported
    // lazily: it pulls in the generated API client, which this store must not
    // depend on (`api/client.ts` depends on it).
    void import("@/lib/push")
      .then((push) => push.dropLocalPush())
      .catch((error: unknown) => {
        console.error(
          "weebo-notify: could not drop the push subscription",
          error,
        );
      });
    const metadata = await discover().catch(() => null);
    // Revoke the refresh token before dropping it: ending the IdP session alone may
    // leave a copy (another tab, or one lifted by script) valid until it expires.
    const token = storedRefreshToken();
    if (token && metadata?.revocation_endpoint) {
      await revoke(metadata.revocation_endpoint, token);
    }
    forget();
    if (metadata?.end_session_endpoint) {
      const url = new URL(metadata.end_session_endpoint);
      url.searchParams.set("client_id", oidcClientId);

      // The IdP cookie lives in the system browser, so desktop logs out there.
      if (isDesktop()) {
        await openExternal(url.toString()).catch(() => {});
      } else {
        url.searchParams.set(
          "post_logout_redirect_uri",
          `${window.location.origin}/`,
        );
        window.location.assign(url.toString());
        return;
      }
    }

    await options?.afterSignOut?.();
    // A reload, not a push, so no component keeps the previous session's data.
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
    pendingAuthorizeUrl,
    bearer,
    canResume,
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
    throw new TokenRefused(response.status, detail);
  }

  return (await response.json()) as TokenResponse;
}
