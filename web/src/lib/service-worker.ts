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

import { router } from "@/router";

/** Where the worker lives. Root scope, which is what lets it serve every route. */
const SCRIPT = "/sw.js";

/** Whether this browser can run one at all. */
export function supported(): boolean {
  return "serviceWorker" in navigator;
}

/**
 * Register it, and listen for what it cannot do alone.
 *
 * Returns the registration so `lib/push.ts` can subscribe against it, or `null`
 * when the browser has no service workers — Firefox in a private window, an
 * insecure origin, a locked-down enterprise profile. All three are ordinary, and
 * none of them is an error to report.
 */
export async function register(
  onSubscriptionChange?: () => void,
): Promise<ServiceWorkerRegistration | null> {
  if (!supported()) {
    return null;
  }

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
        onSubscriptionChange?.();
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
 * `navigator.serviceWorker.ready` never rejects and never resolves without one, so
 * it is raced against nothing: callers that need a worker *now* — subscribing to
 * push is the only one — get `null` instead of hanging when there is none.
 */
export async function ready(): Promise<ServiceWorkerRegistration | null> {
  if (!supported()) {
    return null;
  }
  return navigator.serviceWorker.ready;
}
