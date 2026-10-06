/**
 * Deep links handed over by the desktop shell (§10bis.3quater). The shell emits an
 * event and the front navigates through its own router, so route guards still run.
 */

import { isDesktop } from "./desktop";

/** The event name the shell emits. Shared with `web/src-tauri/src/main.rs`. */
export const DEEP_LINK_EVENT = "deep-link";

/** Built by `DeepLink::path` on the Rust side. */
export interface DeepLinkPayload {
  path: string;
}

/** Re-validated here: any Tauri event emitter could reach this. */
export function pathOf(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const { path } = payload as Partial<DeepLinkPayload>;
  if (typeof path !== "string") {
    return null;
  }

  // Only the two permalink shapes (§10.4), anchored: rejects relative and `//host` paths.
  return /^\/[nc]\/[A-Za-z0-9._-]+$/.test(path) ? path : null;
}

/** Route on every shell link; returns an unlisten fn, or `null` in a browser. Tauri is a lazy chunk. */
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
    // The app is usable without deep links.
    console.debug("deep links unavailable", caught);
    return null;
  }
}
