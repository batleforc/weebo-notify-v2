/**
 * Tests the shipped `public/sw.js` itself, evaluated in a stubbed worker global:
 * envelope → notification, unknown envelopes still show, visible tabs get a hand-off.
 */

import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { beforeEach, describe, expect, it } from "vitest";

// From the project root rather than from `import.meta.url`: under jsdom the module
// URL is an http one, and `fileURLToPath` refuses it.
const SOURCE = readFileSync(resolve(process.cwd(), "public/sw.js"), "utf8");

interface Harness {
  listeners: Map<string, (event: unknown) => void>;
  shown: { title: string; options: Record<string, unknown> }[];
  posted: unknown[];
  windows: { visibilityState: string; url: string }[];
}

/** Load `sw.js` into a fake `ServiceWorkerGlobalScope` and return what it touched. */
function load(language = "fr-FR"): Harness {
  const listeners = new Map<string, (event: unknown) => void>();
  const shown: Harness["shown"] = [];
  const posted: unknown[] = [];
  const windows: Harness["windows"] = [];

  const self = {
    addEventListener: (type: string, handler: (event: unknown) => void) =>
      listeners.set(type, handler),
    skipWaiting: () => Promise.resolve(),
    location: { origin: "https://notify.test" },
    navigator: { language },
    registration: {
      showNotification: (title: string, options: Record<string, unknown>) => {
        shown.push({ title, options });
        return Promise.resolve();
      },
    },
    clients: {
      claim: () => Promise.resolve(),
      matchAll: () =>
        Promise.resolve(
          windows.map((window) => ({
            ...window,
            postMessage: (message: unknown) => posted.push(message),
            focus: () => Promise.resolve({ navigate: () => Promise.resolve() }),
          })),
        ),
      openWindow: () => Promise.resolve(null),
    },
  };

  const caches = {
    open: () =>
      Promise.resolve({
        addAll: () => Promise.resolve(),
        put: () => undefined,
      }),
    keys: () => Promise.resolve([]),
    delete: () => Promise.resolve(true),
    match: () => Promise.resolve(undefined),
  };

  // Indirect evaluation with the globals the worker expects. `Response` and
  // `fetch` are never reached by the handlers under test but have to exist.
  new Function("self", "caches", "fetch", "Response", "URL", SOURCE)(
    self,
    caches,
    () => Promise.reject(new Error("offline")),
    class {},
    URL,
  );

  return { listeners, shown, posted, windows };
}

/** A `push` event carrying `payload` as its decrypted JSON. */
function pushEvent(payload: unknown) {
  const waits: Promise<unknown>[] = [];
  return {
    event: {
      data: payload === undefined ? null : { json: () => payload },
      waitUntil: (promise: Promise<unknown>) => waits.push(promise),
    },
    settled: () => Promise.all(waits),
  };
}

describe("the service worker", () => {
  let harness: Harness;

  beforeEach(() => {
    harness = load();
  });

  it("registers the handlers a push notification needs", () => {
    expect([...harness.listeners.keys()]).toEqual(
      expect.arrayContaining([
        "install",
        "activate",
        "fetch",
        "push",
        "notificationclick",
        "pushsubscriptionchange",
      ]),
    );
  });

  it("shows a notification's own title and links to its permalink", async () => {
    const { event, settled } = pushEvent({
      kind: "notification",
      channel: "ci-infra",
      notification_id: "ntf_01J",
      content: { title: "etcd degraded", body: "quorum lost" },
    });

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown).toHaveLength(1);
    expect(harness.shown[0].title).toBe("etcd degraded");
    expect(harness.shown[0].options.body).toBe("quorum lost");
    expect(harness.shown[0].options.data).toEqual({ url: "/n/ntf_01J" });
    // Per notification, so five revisions of one incident replace each other on
    // the lock screen instead of stacking (§2.1).
    expect(harness.shown[0].options.tag).toBe("notification:ntf_01J");
  });

  it("says what it knows when the payload did not fit", async () => {
    // A poke (§6.3): identifiers and no content, because the payload was over the
    // transport's ceiling. The channel is what is known.
    const { event, settled } = pushEvent({
      kind: "notification",
      channel: "ci-infra",
      notification_id: "ntf_01K",
    });

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown[0].title).toBe("ci-infra");
    expect(harness.shown[0].options.body).toBe("Ouvrez pour voir le détail");
  });

  it("renders a digest as the two numbers it is", async () => {
    const { event, settled } = pushEvent({
      kind: "digest",
      total: 12,
      failures: 2,
    });

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown[0].options.body).toContain("12");
    expect(harness.shown[0].options.body).toContain("2");
    expect(harness.shown[0].options.data).toEqual({ url: "/" });
  });

  it("still shows something for an envelope it cannot read", async () => {
    // `userVisibleOnly`: an unseen push gets the subscription revoked.
    const { event, settled } = pushEvent({ kind: "something-from-2027" });

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown).toHaveLength(1);
    expect(harness.shown[0].title).toBe("weebo-notify");
  });

  it("survives a push with no payload at all", async () => {
    const { event, settled } = pushEvent(undefined);

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown).toHaveLength(1);
  });

  it("hands a push to a visible tab instead of doubling the toast", async () => {
    harness.windows.push({
      visibilityState: "visible",
      url: "https://notify.test/",
    });
    const { event, settled } = pushEvent({
      kind: "notification",
      channel: "ci-infra",
      notification_id: "ntf_01L",
      content: { title: "etcd degraded" },
    });

    harness.listeners.get("push")?.(event);
    await settled();

    // The page raises it, behind the local do-not-disturb the worker cannot read.
    expect(harness.shown).toHaveLength(0);
    expect(harness.posted).toEqual([
      {
        type: "push",
        envelope: expect.objectContaining({
          title: "etcd degraded",
          notification: "ntf_01L",
        }),
      },
    ]);
  });

  it("shows the notification when every tab is hidden", async () => {
    harness.windows.push({
      visibilityState: "hidden",
      url: "https://notify.test/",
    });
    const { event, settled } = pushEvent({
      kind: "notification",
      channel: "ci-infra",
      notification_id: "ntf_01M",
      content: { title: "etcd degraded" },
    });

    harness.listeners.get("push")?.(event);
    await settled();

    // A background tab shows nothing, so treating it as having handled the push
    // would swallow the notification entirely.
    expect(harness.shown).toHaveLength(1);
    expect(harness.posted).toHaveLength(0);
  });

  it("speaks the browser's language when it is not French", async () => {
    harness = load("en-GB");
    const { event, settled } = pushEvent({
      kind: "notification",
      channel: "ci-infra",
      notification_id: "ntf_01N",
    });

    harness.listeners.get("push")?.(event);
    await settled();

    expect(harness.shown[0].options.body).toBe("Open to see the details");
  });

  it("asks whichever tab is open to re-enrol when the subscription rotates", async () => {
    harness.windows.push({
      visibilityState: "hidden",
      url: "https://notify.test/",
    });
    const waits: Promise<unknown>[] = [];

    harness.listeners.get("pushsubscriptionchange")?.({
      waitUntil: (promise: Promise<unknown>) => waits.push(promise),
    });
    await Promise.all(waits);

    // The worker holds no credential, so re-enrolling is the page's job.
    expect(harness.posted).toEqual([{ type: "pushsubscriptionchange" }]);
  });
});
