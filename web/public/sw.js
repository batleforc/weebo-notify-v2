/**
 * The service worker: what runs when the app does not (design §6.2, §6.3, §10.4).
 *
 * Two jobs, and only one of them is offline:
 *
 *   1. **Show a push.** A Web Push arrives whether or not a tab is open, and only
 *      a service worker can display it. The payload it decrypts is the `Envelope`
 *      of `notify-push-webpush` — a wire contract with no OpenAPI entry, because a
 *      push is not an HTTP response.
 *   2. **Serve the shell offline.** Enough for the app to open, show what it has
 *      and say it is offline. The inbox itself is not cached here: the backlog is
 *      the server's answer to `GET /sync`, and a second, staler copy in a cache
 *      would be a source of truth nobody asked for.
 *
 * # Written by hand rather than generated
 *
 * The obvious move is `vite-plugin-pwa`, which brings Workbox. It is refused for
 * the same reason ADR-0004 vendors shadcn components: a precache manifest generated
 * at build time is a build-time dependency injected into a file that has to be
 * *read* to be trusted, and this one decides what a person sees on a locked screen.
 * What it buys — hashed precaching and cache versioning — is the thirty lines below.
 *
 * The cost is stated so nobody is surprised: this file is plain JavaScript, served
 * verbatim from `public/`, so it is neither bundled nor type-checked. It is
 * deliberately small enough to read in one sitting, and `src/lib/sw.spec.ts` runs
 * *this file* inside a stubbed worker global rather than a copy of it — which is
 * the only kind of test worth having about an artefact that ships unprocessed.
 *
 * # Never take over a page it was not built for
 *
 * `CACHE` carries the shell version. Anything under another name is dropped on
 * activate, which is what stops a stale bundle outliving a deploy — the failure
 * everybody meets once and debugs by clearing site data.
 */

/// <reference lib="webworker" />

const CACHE = "weebo-notify-shell-v1";

/**
 * How many responses the cache keeps before dropping the least recently fetched.
 *
 * The name above never changes between deploys, and Vite's asset names do — so
 * without a bound every build's hashed chunks would pile up under it for ever.
 * `put` re-appends an entry it replaces, so the oldest keys are the ones no
 * current page has asked for in the longest time.
 */
const KEEP = 60;

/**
 * The strings this worker shows on its own, in the two locales the app ships
 * (`src/locales`). The worker has no access to the app's i18n, only to the
 * browser's language.
 */
const TEXT = {
  fr: {
    digestTitle: "Pendant votre absence",
    digest: (total) => `${total} notification(s)`,
    digestFailures: (total, failures) =>
      `${total} notification(s), dont ${failures} en échec`,
    poke: "Ouvrez pour voir le détail",
    unknown: "Une notification est arrivée",
  },
  en: {
    digestTitle: "While you were away",
    digest: (total) => `${total} notification(s)`,
    digestFailures: (total, failures) =>
      `${total} notification(s), ${failures} failed`,
    poke: "Open to see the details",
    unknown: "A notification arrived",
  },
};

/** French unless the browser asks for English — the app's own fallback is `fr`. */
function text() {
  const language = self.navigator?.language ?? "fr";
  return language.toLowerCase().startsWith("en") ? TEXT.en : TEXT.fr;
}

/**
 * What has to be there for the app to open with no network.
 *
 * The document alone: Vite emits hashed asset names, so listing them here would
 * mean regenerating this file on every build — the very build coupling refused
 * above. They land in the cache the first time they are fetched instead, which
 * costs one online visit before the app works offline and nothing after.
 */
const SHELL = ["/", "/index.html", "/icon.svg", "/manifest.webmanifest"];

self.addEventListener("install", (event) => {
  // `skipWaiting` so a reload is enough to pick up a new worker. Without it the
  // new one idles until every tab is closed, and "I refreshed and nothing
  // changed" is how a service worker earns its reputation.
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(
          names
            .filter((name) => name !== CACHE)
            .map((name) => caches.delete(name)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

/**
 * Network first, cache as a fallback — and only for this origin's own assets.
 *
 * The other way round (cache first) is faster and wrong here: it serves a stale
 * bundle to somebody who is online, and the whole product is about seeing what
 * happened *now*. API calls are never touched at all: a cached `GET /sync` would
 * hand the app a backlog the cursor has already moved past.
 */
self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") {
    return;
  }

  const url = new URL(request.url);
  if (url.origin !== self.location.origin || url.pathname.startsWith("/api/")) {
    return;
  }

  event.respondWith(
    fetch(request)
      .then((response) => {
        // Opaque and error responses are not worth keeping: replaying a 502 from
        // the cache is worse than being offline, which at least says so.
        if (response.ok && response.type === "basic") {
          const copy = response.clone();
          caches.open(CACHE).then(async (cache) => {
            await cache.put(request, copy);
            const keys = await cache.keys();
            await Promise.all(
              keys
                .slice(0, Math.max(0, keys.length - KEEP))
                .filter((key) => !SHELL.includes(new URL(key.url).pathname))
                .map((key) => cache.delete(key)),
            );
          });
        }
        return response;
      })
      .catch(async () => {
        const cached = await caches.match(request);
        if (cached) {
          return cached;
        }
        // A navigation to a route the cache has never seen — every permalink, on
        // a first offline visit. The SPA shell knows how to render it.
        if (request.mode === "navigate") {
          const shell = await caches.match("/index.html");
          if (shell) {
            return shell;
          }
        }
        return Response.error();
      }),
  );
});

/**
 * A push arrived.
 *
 * `userVisibleOnly` was promised at subscription time, so this handler shows
 * something on every push — a browser that catches one staying silent revokes the
 * subscription. That is why the catch-all in `readEnvelope` still renders: an
 * envelope this build cannot read is a *newer server*, and going quiet would be
 * the worst of both answers.
 *
 * The one exception is a tab that is **visible**, and it is not a liberty taken:
 * a browser with the app open receives the same arrival twice — once on its own
 * stream (§8.1) and once as a push — so showing an OS notification here would
 * duplicate the toast the page is already about to raise. Handing it to the page
 * instead is also what puts it behind the local do-not-disturb of §7.4, which the
 * worker cannot evaluate: it holds no session and no preference.
 */
self.addEventListener("push", (event) => {
  const envelope = readEnvelope(event.data);
  event.waitUntil(
    handedToAVisibleTab(envelope).then((handed) => {
      if (handed) {
        return undefined;
      }
      return show(envelope);
    }),
  );
});

/**
 * Give it to a visible tab, and say whether one took it.
 *
 * "Visible" and not merely "open": a background tab shows nothing, so treating it
 * as having handled the push would swallow the notification entirely.
 */
function handedToAVisibleTab(envelope) {
  return self.clients
    .matchAll({ type: "window", includeUncontrolled: true })
    .then((windows) => {
      const visible = windows.filter(
        (client) => client.visibilityState === "visible",
      );
      if (visible.length === 0) {
        return false;
      }
      for (const client of visible) {
        client.postMessage({ type: "push", envelope });
      }
      return true;
    });
}

/** Put it on the lock screen. */
function show(envelope) {
  return self.registration.showNotification(envelope.title, {
    body: envelope.body,
    icon: "/icon.svg",
    badge: "/icon.svg",
    // Same tag ⇒ the OS replaces rather than stacks, which is what makes a
    // notification a lifecycle object here and not a log line (§2.1): five
    // revisions of one incident are one entry on the lock screen.
    tag: envelope.tag,
    data: { url: envelope.url },
    // A revision that supersedes what is already on screen should not buzz a
    // second time; a new one should.
    renotify: envelope.renotify,
  });
}

/**
 * Turn the decrypted payload into what the OS shows.
 *
 * Mirrors `Envelope` in `crates/notify-adapters/push-webpush`: `kind` is the tag,
 * and a `notification` without `content` is the poke of §6.3 — the payload did not
 * fit, so the app fetches over TLS when it opens. It is deliberately NOT fetched
 * here: doing so would need a credential the worker does not hold, and would turn
 * every large notification into a request before anything is shown.
 */
function readEnvelope(data) {
  let payload = null;
  try {
    payload = data ? data.json() : null;
  } catch {
    payload = null;
  }

  if (payload && payload.kind === "digest") {
    const total = payload.total ?? 0;
    const failures = payload.failures ?? 0;
    return {
      title: text().digestTitle,
      body:
        failures > 0
          ? text().digestFailures(total, failures)
          : text().digest(total),
      tag: "digest",
      url: "/",
      renotify: true,
    };
  }

  if (payload && payload.kind === "notification") {
    const content = payload.content ?? null;
    const id = payload.notification_id ?? "";
    return {
      title: content?.title ?? payload.channel ?? "weebo-notify",
      // No content means a poke: say what is known rather than invent a body.
      body: content?.body ?? (content ? "" : text().poke),
      // Per notification, so revisions of the same one replace each other.
      tag: id ? `notification:${id}` : "notification",
      url: id ? `/n/${id}` : "/",
      // For the page: the silence check and the dedupe against the stream's own
      // toast both key on it (`stores/toasts.ts`).
      notification: id || undefined,
      renotify: true,
    };
  }

  // An envelope this build does not know. Showing something is not politeness —
  // it is the `userVisibleOnly` contract.
  return {
    title: "weebo-notify",
    body: text().unknown,
    tag: "unknown",
    url: "/",
    renotify: false,
  };
}

/**
 * Clicking one opens what it is about.
 *
 * An already-open tab is focused and navigated rather than joined by a second one:
 * somebody who clicks a notification wants *the* app, not another copy of it.
 */
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = new URL(
    event.notification.data?.url || "/",
    self.location.origin,
  );

  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((windows) => {
        for (const client of windows) {
          if (new URL(client.url).origin === target.origin) {
            return client.focus().then((focused) =>
              // `navigate` is not available on every client handle; falling back
              // to a message lets the app route itself.
              focused.navigate
                ? focused.navigate(target.href)
                : focused.postMessage({
                    type: "navigate",
                    url: target.pathname,
                  }),
            );
          }
        }
        return self.clients.openWindow(target.href);
      }),
  );
});

/**
 * The push service rotated the subscription under us.
 *
 * It happens without anybody doing anything, and the old endpoint stops working
 * immediately. The worker cannot re-enrol on its own — that needs a credential it
 * does not hold — so it tells whichever tab is open, and `lib/push.ts` re-enrols
 * with the session it has. A closed app catches up at the next launch, which is
 * also when it would have noticed.
 */
self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((windows) => {
        for (const client of windows) {
          client.postMessage({ type: "pushsubscriptionchange" });
        }
      }),
  );
});
