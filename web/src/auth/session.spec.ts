/**
 * Where the refresh token rests per platform (§10bis.2). Each test checks the
 * right store was written and the other left alone.
 */
import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** `session.ts`'s own constant, which it does not export. */
const REFRESH_KEY = "weebo-notify:refresh-token";

const { desktop, credential, push } = vi.hoisted(() => ({
  // A holder rather than a bare boolean: `session.ts` reads `isDesktop()` at module
  // scope for `redirectUri`, so the value has to be settable before the import.
  desktop: { value: true },
  credential: {
    save: vi.fn(),
    load: vi.fn(),
    clear: vi.fn(),
    share: vi.fn(),
  },
  push: { disable: vi.fn(), dropLocalPush: vi.fn(), claimFor: vi.fn() },
}));

vi.mock("@/lib/push", () => push);

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

/** A fresh store per platform: `redirectUri` is fixed at module load. */
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
    push.disable.mockReset().mockResolvedValue(undefined);
    push.dropLocalPush.mockReset().mockResolvedValue(undefined);
    push.claimFor.mockReset().mockResolvedValue(undefined);
    sessionStorage.clear();
    localStorage.clear();
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

      // The access token (what the tray runs on); no name without an `id_token`.
      expect(credential.share).toHaveBeenCalledWith("access-2", null);
    });

    it("sends the display name the claims carry, for the tray header", async () => {
      // Unsigned: this suite doesn't test JWT verification.
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

    it("keeps the refresh token through a network failure", async () => {
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => {
          throw new TypeError("Failed to fetch");
        }),
      );
      const store = await session(false);

      // Offline is not signed out: the router lets the app load, and the next
      // request tries the refresh again.
      expect(await store.restore()).toBe(true);
      expect(store.expired).toBe(false);
      expect(sessionStorage.getItem(REFRESH_KEY)).toBe("refresh-1");
    });

    it("keeps it through a provider error that is not a refusal", async () => {
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => new Response("down", { status: 503 })),
      );
      const store = await session(false);

      expect(await store.refresh()).toBeNull();
      expect(sessionStorage.getItem(REFRESH_KEY)).toBe("refresh-1");
    });

    /** The provider refuses every refresh: the session dies without a sign-out. */
    function refuseRefresh() {
      vi.stubGlobal(
        "fetch",
        vi.fn(
          async () =>
            new Response('{"error":"invalid_grant"}', { status: 400 }),
        ),
      );
    }

    /**
     * An in-memory `BroadcastChannel`, per name and asynchronous like the real one,
     * so a test controls exactly which other "tabs" exist.
     */
    function fakeChannels() {
      const open: {
        name: string;
        onmessage: ((e: MessageEvent) => void) | null;
      }[] = [];
      vi.stubGlobal(
        "BroadcastChannel",
        class {
          onmessage: ((e: MessageEvent) => void) | null = null;
          constructor(readonly name: string) {
            open.push(this);
          }
          postMessage(data: unknown) {
            for (const other of open) {
              if (other !== this && other.name === this.name) {
                queueMicrotask(() =>
                  other.onmessage?.({ data } as MessageEvent),
                );
              }
            }
          }
        },
      );
    }

    /** Long enough for the presence question to have timed out. */
    const pastTimeout = () =>
      new Promise((resolve) => setTimeout(resolve, 400));

    it("ends the session when the provider refuses the token", async () => {
      fakeChannels();
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      refuseRefresh();
      const store = await session(false);

      expect(await store.refresh()).toBeNull();
      expect(store.expired).toBe(true);
      expect(sessionStorage.getItem(REFRESH_KEY)).toBeNull();
      // The only signed-in tab is gone: nothing else can revoke this browser's
      // push, so it is dropped locally (the push service's `410` does the rest).
      await vi.waitFor(() => expect(push.dropLocalPush).toHaveBeenCalled());
      expect(push.disable).not.toHaveBeenCalled();
    });

    it("keeps push when another signed-in tab answers", async () => {
      fakeChannels();
      const peer = new BroadcastChannel("weebo-notify:presence");
      const heard: unknown[] = [];
      peer.onmessage = (event) => {
        heard.push(event.data);
        if (event.data === "ping") peer.postMessage("pong");
      };
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      refuseRefresh();
      const store = await session(false);

      await store.refresh();
      await pastTimeout();

      expect(heard).toContain("ping");
      expect(push.dropLocalPush).not.toHaveBeenCalled();
    });

    it("falls back to a localStorage heartbeat without BroadcastChannel", async () => {
      vi.stubGlobal("BroadcastChannel", undefined);
      localStorage.setItem("weebo-notify:presence:peer", String(Date.now()));
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      refuseRefresh();
      const store = await session(false);

      await store.refresh();
      await pastTimeout();
      expect(push.dropLocalPush).not.toHaveBeenCalled();

      // A heartbeat gone stale is a tab that crashed, not one signed in.
      localStorage.clear(); // the first store's own beat, a "tab" of its own here
      localStorage.setItem("weebo-notify:presence:peer", "1");
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      await (await session(false)).refresh();
      await vi.waitFor(() => expect(push.dropLocalPush).toHaveBeenCalled());
    });

    it("tears push down on an explicit sign-out", async () => {
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      const store = await session(false);
      await store.restore();

      await store.logout();

      // Revoked server-side while the token still works, then dropped locally.
      expect(push.disable).toHaveBeenCalled();
      await vi.waitFor(() => expect(push.dropLocalPush).toHaveBeenCalled());
    });

    it("hands a rotated token to a duplicated tab holding the old one", async () => {
      // "Duplicate tab" copies sessionStorage, so both tabs hold refresh-1.
      const channels: { onmessage: ((e: MessageEvent) => void) | null }[] = [];
      vi.stubGlobal(
        "BroadcastChannel",
        class {
          onmessage: ((e: MessageEvent) => void) | null = null;
          constructor() {
            channels.push(this);
          }
          postMessage(data: unknown) {
            for (const other of channels) {
              if (other !== this) {
                other.onmessage?.({ data } as MessageEvent);
              }
            }
          }
        },
      );
      sessionStorage.setItem(REFRESH_KEY, "refresh-1");
      const store = await session(false);
      // The twin tab: another module instance whose storage still says refresh-1.
      const twin = channels.length;
      await session(false);
      const received: unknown[] = [];
      const channel = channels[twin];
      if (!channel) {
        throw new Error("the twin tab opened no channel");
      }
      const original = channel.onmessage;
      channel.onmessage = (event) => {
        received.push(event.data);
        original?.(event);
      };

      await store.refresh();

      expect(received).toEqual([{ from: "refresh-1", to: "refresh-2" }]);
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
