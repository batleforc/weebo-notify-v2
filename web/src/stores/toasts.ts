/**
 * The local popups §7.4 talks about, and the rule that silences them.
 *
 * An open tab learns about an arrival twice over — on its own stream (§8.1), and
 * as a Web Push handed over by the service worker when a window is visible. Both
 * land here, both go through the same gate, and only the first of the two raises
 * a toast:
 *
 *   > Les clients connectés respectent le DND localement […] pendant un DND (ou un
 *   > `silence` actif), ils suppriment ces popups tout en continuant d'alimenter
 *   > l'inbox. Sans cette règle, le SSE contournerait le DND sur les clients
 *   > ouverts.
 *
 * "Tout en continuant d'alimenter l'inbox" is the half that is easy to lose: this
 * store decides whether something is *shown*, never whether it is *kept*. The
 * inbox store folds every event regardless, so a muted arrival is in the list the
 * moment somebody looks — which is exactly the difference between quiet and lost.
 */

import { defineStore } from "pinia";
import { ref } from "vue";

import type { Event as StreamEvent } from "@/api/generated";
import { usePreferences } from "@/stores/preferences";

/** How long one stays before it fades, in milliseconds. */
const LINGER = 6_000;

/** The most on screen at once, oldest dropped first. */
const AT_MOST = 4;

/**
 * How long one notification's toast suppresses another for it.
 *
 * A visible tab hears of an arrival twice — its stream and the push the worker
 * hands over — in whichever order the network delivers them. One toast per
 * notification per window is the answer to both.
 */
const DEDUPE_MS = 10_000;

export interface Toast {
  /** Stable across the toast's life, so a re-render does not restart it. */
  id: string;
  title: string;
  body?: string;
  /** Where clicking it goes — the permalink of §10.4. */
  url?: string;
}

let sequence = 0;

export const useToasts = defineStore("toasts", () => {
  const items = ref<Toast[]>([]);
  const timers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Notification id → when its last toast went up. */
  const recent = new Map<string, number>();

  function dismiss(id: string): void {
    clearTimeout(timers.get(id));
    timers.delete(id);
    items.value = items.value.filter((toast) => toast.id !== id);
  }

  /**
   * Hold one on screen while it is hovered or focused — six seconds is not long
   * enough to read with a screen magnifier or reach with a keyboard (WCAG 2.2.1).
   */
  function hold(id: string): void {
    clearTimeout(timers.get(id));
    timers.delete(id);
  }

  /** Start its countdown again once the pointer or focus has left it. */
  function release(id: string): void {
    if (!timers.has(id) && items.value.some((toast) => toast.id === id)) {
      // Timers are per toast rather than one sweep: a stack that all disappears
      // at once reads as a bug, and dismissing one by hand must not shorten the
      // rest.
      timers.set(
        id,
        setTimeout(() => dismiss(id), LINGER),
      );
    }
  }

  /**
   * Show one. The do-not-disturb gate is in `arrival`/`pushed` below; this only
   * refuses a second toast for a notification that just had one.
   */
  function push(
    toast: Omit<Toast, "id">,
    notification?: string,
  ): string | null {
    if (notification) {
      const now = Date.now();
      for (const [key, at] of recent) {
        if (now - at >= DEDUPE_MS) {
          recent.delete(key);
        }
      }
      if (recent.has(notification)) {
        return null;
      }
      recent.set(notification, now);
    }

    sequence += 1;
    const id = `toast-${sequence}`;
    const kept = [...items.value, { ...toast, id }];
    for (const dropped of kept.slice(0, -AT_MOST)) {
      hold(dropped.id);
    }
    items.value = kept.slice(-AT_MOST);
    release(id);
    return id;
  }

  /**
   * A revision arrived on the stream.
   *
   * Only `revision` raises anything: a receipt or an interaction is somebody
   * *else's* activity on something already seen, and a popup for each would make
   * a busy channel unusable — they still reach the detail panel, which is where
   * they mean something.
   */
  function arrival(event: StreamEvent): void {
    if (event.event !== "revision" || event.audience !== "channel") {
      return;
    }

    const preferences = usePreferences();
    if (
      preferences.stayQuiet({
        notification: event.notification_id,
        groupKey: event.payload.group_key ?? null,
        mutedByDnd: event.muted_by_dnd ?? false,
      })
    ) {
      return;
    }

    push(
      {
        title: event.payload.title,
        body: event.payload.body ?? undefined,
        url: `/n/${event.notification_id}`,
      },
      event.notification_id,
    );
  }

  /**
   * A push the service worker handed over because this tab is visible.
   *
   * Through the same gate, and that is the point: the worker skipped the OS
   * notification precisely because it cannot evaluate a preference, so if this
   * did not check, a push would walk past a do-not-disturb the stream respects.
   *
   * A poke (`content` absent, §6.3) carries no title of its own — the channel is
   * what is known, and the inbox has the rest a click away.
   *
   * Do-not-disturb itself was applied by the server before it sent the push
   * (D22); what is left to check here is the local half — silences.
   */
  function pushed(envelope: {
    kind?: string;
    title?: string;
    body?: string;
    url?: string;
    notification?: string;
  }): void {
    if (!envelope.title) {
      return;
    }
    if (
      envelope.notification &&
      usePreferences().stayQuiet({ notification: envelope.notification })
    ) {
      return;
    }
    push(
      { title: envelope.title, body: envelope.body, url: envelope.url },
      envelope.notification,
    );
  }

  return { items, arrival, push, pushed, dismiss, hold, release };
});
