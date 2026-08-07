/**
 * Where the shell sends the front end when the OS hands over a link
 * (§10bis.3quater).
 *
 * # Why an event and not the shell driving the router
 *
 * The first scaffold had the Rust side `eval` a `pushState` into the webview. It
 * works, and it is the crudest thing that does: it reaches past the router into the
 * History API, so the router only learns about the navigation because a synthetic
 * `popstate` is fired after it, and any guard the router runs is bypassed. That is a
 * bug waiting for the first authenticated deep link — `/n/<id>` opened signed out is
 * supposed to route through the login guard and come back (see `router/index.ts`).
 *
 * So the shell emits, the front listens, and the front navigates with its own
 * router. The parsing and the refusal stay in `notify_client_core::deeplink`, which
 * is the side that receives untrusted input from the OS; what crosses to here is
 * already one of two known shapes.
 */

import { isDesktop } from "./desktop";

/** The event name the shell emits. Shared with `web/src-tauri/src/main.rs`. */
export const DEEP_LINK_EVENT = "deep-link";

/**
 * What the shell sends: the web path the link mirrors, produced by
 * `DeepLink::path` on the Rust side rather than rebuilt here.
 */
export interface DeepLinkPayload {
  path: string;
}

/**
 * Accept a payload, or refuse it.
 *
 * The Rust side already validated the id, and this validates again anyway. Not
 * belt-and-braces for its own sake: this function is also reachable from anything
 * else that can emit a Tauri event into this webview, and "the other side checked"
 * is a sentence that stops being true the moment there is a second other side.
 */
export function pathOf(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const { path } = payload as Partial<DeepLinkPayload>;
  if (typeof path !== "string") {
    return null;
  }

  // Exactly the two permalink shapes of §10.4, anchored at both ends. A relative
  // path would be resolved against wherever the app currently is, and a protocol
  // -relative one (`//host`) is an absolute URL wearing a path's clothes.
  return /^\/[nc]\/[A-Za-z0-9._-]+$/.test(path) ? path : null;
}

/**
 * Route the app whenever the shell reports a link. A no-op in a browser.
 *
 * `@tauri-apps/api` is imported **dynamically**, and that is the whole reason this
 * is a function rather than a top-level import: Vite emits it as its own chunk, so a
 * browser never fetches a byte of a desktop package. It is a `devDependency` for the
 * same reason — nothing in the deployed web bundle depends on it.
 *
 * Returns a function that stops listening, or `null` when there was nothing to
 * listen to.
 */
export async function onDeepLink(
  go: (path: string) => void,
): Promise<(() => void) | null> {
  if (!isDesktop()) {
    return null;
  }

  try {
    const { listen } = await import("@tauri-apps/api/event");
    return await listen(DEEP_LINK_EVENT, (event) => {
      const path = pathOf(event.payload);
      if (path) {
        go(path);
      }
    });
  } catch (caught) {
    // A shell that cannot deliver deep links is still a usable app: the inbox, the
    // stream and every other surface work. Failing to mount this listener must not
    // take the window down with it.
    console.debug("deep links unavailable", caught);
    return null;
  }
}
