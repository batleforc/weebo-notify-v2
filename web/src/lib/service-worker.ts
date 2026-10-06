/**
 * Service worker registration (PWA, offline). Separate from `lib/push.ts`, and never
 * awaited by rendering: the app works without a worker.
 */

import { isDesktop } from "@/lib/desktop";
import { router } from "@/router";

/** Where the worker lives. Root scope, which is what lets it serve every route. */
const SCRIPT = "/sw.js";

/** `serviceWorker.ready` never settles if registration failed. */
const READY_TIMEOUT_MS = 10_000;

let registering: Promise<ServiceWorkerRegistration | null> | null = null;

/** What to call when the push subscription rotates — the latest `register` wins. */
let onRotation: (() => void) | undefined;

/** Never in the desktop shell: `tauri://` can't host a worker (§10bis). */
export function supported(): boolean {
  return !isDesktop() && "serviceWorker" in navigator;
}

/** `null` when the browser has no service workers (private window, insecure origin): not an error. */
export function register(
  onSubscriptionChange?: () => void,
): Promise<ServiceWorkerRegistration | null> {
  onRotation = onSubscriptionChange;
  if (!supported()) {
    return Promise.resolve(null);
  }
  // Once per page: AppLayout remounts after sign-out.
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
      // A notification click the worker couldn't navigate itself.
      if (message.type === "navigate" && message.url) {
        void router.push(message.url);
      }
      // Only a page holds the credential needed to re-enrol.
      if (message.type === "pushsubscriptionchange") {
        onRotation?.();
      }
    });

    return registration;
  } catch (error) {
    console.warn("service worker registration failed", error);
    return null;
  }
}

/** The registration once ready, or `null` on failure/timeout instead of hanging. */
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
