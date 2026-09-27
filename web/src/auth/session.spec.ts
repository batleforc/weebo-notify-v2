/**
 * Where the refresh token rests, per platform — the one thing this store now does
 * differently in the shell than in a browser.
 *
 * Worth its own file because the split is easy to undo by accident and expensive
 * both ways. Collapse it towards the browser and the shell stops surviving a
 * restart, which is the whole reason `./credential` exists (§10bis.2, §10bis.3).
 * Collapse it the other way and a *browser* build starts calling Tauri commands
 * that are not there — and because the calls are fire-and-forget, it would fail in
 * a console message rather than anywhere a person looks.
 *
 * So each test asserts both halves: that the right store was written, and that the
 * other one was left alone.
 */
import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** `session.ts`'s own constant, which it does not export. */
const REFRESH_KEY = "weebo-notify:refresh-token";

const { desktop, credential } = vi.hoisted(() => ({
  // A holder rather than a bare boolean: `session.ts` reads `isDesktop()` at module
  // scope for `redirectUri`, so the value has to be settable before the import.
  desktop: { value: true },
  credential: {
    save: vi.fn(),
    load: vi.fn(),
    clear: vi.fn(),
    share: vi.fn(),
  },
}));

vi.mock("@/lib/desktop", () => ({
  isDesktop: () => desktop.value,
  markDesktop: () => desktop.value,
}));

vi.mock("./credential", () => ({
  saveCredential: credential.save,
  loadCredential: credential.load,
  clearCredential: credential.clear,
  shareSessionToken: credential.share,
}));

vi.mock("./discovery", () => ({
  discover: async () => ({ token_endpoint: "https://idp.test/token" }),
}));

/** The provider's answer to a refresh: rotation, so the token that comes back differs. */
function rotated(): Response {
  return new Response(
    JSON.stringify({
      access_token: "access-2",
      refresh_token: "refresh-2",
      token_type: "Bearer",
      expires_in: 900,
    }),
    { status: 200, headers: { "content-type": "application/json" } },
  );
}

/**
 * A fresh store on the given platform.
 *
 * `resetModules` before each import because `redirectUri` is decided once, at module
 * load, from `isDesktop()` — a store imported under the other platform would carry
 * the wrong one.
 */
async function session(onDesktop: boolean) {
  desktop.value = onDesktop;
  vi.resetModules();
  setActivePinia(createPinia());
  const { useSession } = await import("./session");
  return useSession();
}

describe("session — where the refresh token rests", () => {
  beforeEach(() => {
    credential.save.mockReset().mockResolvedValue(undefined);
    credential.load.mockReset().mockResolvedValue(null);
    credential.clear.mockReset().mockResolvedValue(undefined);
    credential.share.mockReset().mockResolvedValue(undefined);
    sessionStorage.clear();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => rotated()),
    );
    // The failure paths below log deliberately; the suite should not be noisy.
    vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  describe("in the shell", () => {
    it("resumes from the vault across a restart, and never touches sessionStorage", async () => {
      credential.load.mockResolvedValue("refresh-1");
      const store = await session(true);

      expect(await store.restore()).toBe(true);

      expect(credential.load).toHaveBeenCalled();
      // Rotation (§5.1): what the provider handed back replaces what was stored.
      expect(credential.save).toHaveBeenCalledWith("refresh-2");
      expect(sessionStorage.getItem(REFRESH_KEY)).toBeNull();
    });

    it("has no session to resume when the vault holds nothing", async () => {
      credential.load.mockResolvedValue(null);
      const store = await session(true);

      expect(await store.restore()).toBe(false);
      // The ordinary first launch: no refresh was attempted, so nothing was stored.
      expect(fetch).not.toHaveBeenCalled();
      expect(credential.save).not.toHaveBeenCalled();
    });

    it("starts on the login screen rather than failing when the vault is unreachable", async () => {
      credential.load.mockRejectedValue(new Error("no secret service"));
      const store = await session(true);

      // Not a throw: a machine with no keyring daemon still runs the app (§9.3bis).
      expect(await store.restore()).toBe(false);
    });

    it("keeps the session usable when the vault refuses the write", async () => {
      credential.load.mockResolvedValue("refresh-1");
      credential.save.mockRejectedValue(new Error("vault refused"));
      const store = await session(true);

      // The token in memory is intact and works; it simply will not outlive the
      // process. Persisting is best-effort, never a precondition.
      expect(await store.restore()).toBe(true);
      expect(store.authenticated).toBe(true);
    });

    it("hands the access token down to the Rust half on every rotation", async () => {
      credential.load.mockResolvedValue("refresh-1");
      const store = await session(true);

      await store.restore();

      // Not the refresh token: this is what a `Bearer` header carries, and it is
      // what the tray runs on. Without it the tray sits on `auth.invalid_credential`
      // for the whole run however healthy the window is.
      // The name rides with it, for the tray's header line. `null` here because
      // this refresh response carries no `id_token`, so there are no claims to
      // read a display name from — which is the shape the Rust side treats as
      // "no name" rather than an empty one.
      expect(credential.share).toHaveBeenCalledWith("access-2", null);
    });

    it("sends the display name the claims carry, for the tray header", async () => {
      // A provider that also returns an `id_token`, which is where the name comes
      // from. Unsigned on purpose: `readClaims` reads the payload and this suite
      // is not testing JWT verification.
      const payload = btoa(
        JSON.stringify({ sub: "alice", name: "Alice Martin" }),
      )
        .replace(/\+/g, "-")
        .replace(/\//g, "_")
        .replace(/=+$/, "");
      vi.stubGlobal(
        "fetch",
        vi.fn(
          async () =>
            new Response(
              JSON.stringify({
                access_token: "access-2",
                refresh_token: "refresh-2",
                id_token: `header.${payload}.signature`,
                token_type: "Bearer",
                expires_in: 900,
              }),
              { status: 200, headers: { "content-type": "application/json" } },
            ),
        ),
      );
      credential.load.mockResolvedValue("refresh-1");
      const store = await session(true);

      await store.restore();

      expect(credential.share).toHaveBeenCalledWith("access-2", "Alice Martin");
    });

    it("tells the Rust half to drop the credential on sign-out", async () => {
      credential.load.mockResolvedValue("refresh-1");
      const store = await session(true);
      await store.restore();
      credential.share.mockClear();

      await store.logout();

      expect(credential.share).toHaveBeenCalledWith(null, null);
    });

    it("keeps the window working when the shell will not take the token", async () => {
      credential.load.mockResolvedValue("refresh-1");
      credential.share.mockRejectedValue(new Error("no credential slot"));
      const store = await session(true);

      // What degrades is the tray, which keeps its last reading. The window is
      // unaffected, so this must not surface as a failed sign-in.
      expect(await store.restore()).toBe(true);
      expect(store.authenticated).toBe(true);
    });

    it("clears the vault on sign-out", async () => {
      credential.load.mockResolvedValue("refresh-1");
      const store = await session(true);
      await store.restore();

      await store.logout();

      expect(credential.clear).toHaveBeenCalled();
      expect(store.authenticated).toBe(false);
    });
  });

  describe("in a browser", () => {
    it("keeps using sessionStorage and never calls a Tauri command", async () => {
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      const store = await session(false);

      expect(await store.restore()).toBe(true);

      expect(sessionStorage.getItem(REFRESH_KEY)).toBe("refresh-2");
      expect(credential.load).not.toHaveBeenCalled();
      expect(credential.save).not.toHaveBeenCalled();
      // There is no Rust half in a browser to hand anything to.
      expect(credential.share).not.toHaveBeenCalled();
    });

    it("does not reach for the vault on sign-out either", async () => {
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      const store = await session(false);
      await store.restore();

      await store.logout();

      expect(credential.clear).not.toHaveBeenCalled();
      expect(sessionStorage.getItem(REFRESH_KEY)).toBeNull();
    });
  });
});
