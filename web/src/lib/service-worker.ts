/**
 * Registering the service worker, and the two messages it sends back.
 *
 * Kept apart from `lib/push.ts` because they answer different questions: this one
 * is "is the app installable and does it open offline", that one is "may we wake
 * this person up". A browser that refuses notifications still gets the PWA.
 *
 * Registration is deliberately not awaited by anything that renders — the app
 * works without a worker, and blocking the first paint on one would trade the
 * thing people notice for the thing they do not.
 */

import { isDesktop } from "@/lib/desktop";
import { router } from "@/router";

/** Where the worker lives. Root scope, which is what lets it serve every route. */
const SCRIPT = "/sw.js";

/**
 * How long `ready()` waits for a worker before answering `null`.
 *
 * `navigator.serviceWorker.ready` never settles when registration failed, and a
 * settings panel awaiting it would say "checking…" for ever.
 */
const READY_TIMEOUT_MS = 10_000;

/** The registration in flight, once `register` has been called. */
let registering: Promise<ServiceWorkerRegistration | null> | null = null;

/** What to call when the push subscription rotates — the latest `register` wins. */
let onRotation: (() => void) | undefined;

/**
 * Whether this browser can run one at all.
 *
 * Never in the desktop shell: its `tauri://` origin is not one a worker can be
 * registered on, and it has native notifications and a tray of its own (§10bis).
 */
export function supported(): boolean {
  return !isDesktop() && "serviceWorker" in navigator;
}

/**
 * Register it, and listen for what it cannot do alone.
 *
 * Returns the registration so `lib/push.ts` can subscribe against it, or `null`
 * when the browser has no service workers — Firefox in a private window, an
 * insecure origin, a locked-down enterprise profile. All three are ordinary, and
 * none of them is an error to report.
 */
export function register(
  onSubscriptionChange?: () => void,
): Promise<ServiceWorkerRegistration | null> {
  onRotation = onSubscriptionChange;
  if (!supported()) {
    return Promise.resolve(null);
  }
  // Once per page: `AppLayout` mounts again after a sign-out, and each call used
  // to stack one more `message` listener on the container.
  registering ??= registerOnce();
  return registering;
}

async function registerOnce(): Promise<ServiceWorkerRegistration | null> {
  try {
    const registration = await navigator.serviceWorker.register(SCRIPT, {
      scope: "/",
    });

    navigator.serviceWorker.addEventListener("message", (event) => {
      const message = event.data as { type?: string; url?: string } | null;
      if (!message) {
        return;
      }
      // A click landed on a tab whose handle the worker could not navigate. The
      // router is the only thing that knows how to open a permalink in place.
      if (message.type === "navigate" && message.url) {
        void router.push(message.url);
      }
      // The push service rotated the subscription out from under us; only a page
      // holds the credential needed to enrol the replacement.
      if (message.type === "pushsubscriptionchange") {
        onRotation?.();
      }
    });

    return registration;
  } catch (error) {
    // A failed registration costs offline support and nothing else, so it is a
    // console line rather than something in the user's way.
    console.warn("service worker registration failed", error);
    return null;
  }
}

/**
 * The registration, once it is ready.
 *
 * `navigator.serviceWorker.ready` never rejects and never resolves without one,
 * so it is raced against a failed registration and a timeout: callers that need a
 * worker *now* — subscribing to push is the only one — get `null` instead of
 * hanging when there is none.
 */
export async function ready(): Promise<ServiceWorkerRegistration | null> {
  if (!supported()) {
    return null;
  }
  if (registering && !(await registering)) {
    return null;
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), READY_TIMEOUT_MS);
  });
  try {
    return await Promise.race([navigator.serviceWorker.ready, timeout]);
  } finally {
    clearTimeout(timer);
  }
}
