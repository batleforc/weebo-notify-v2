/**
 * The local popups §7.4 talks about, and the rule that silences them.
 *
 * An open tab learns about an arrival twice over — on its own stream (§8.1), and
 * as a Web Push handed over by the service worker when a window is visible. Both
 * land here, and both go through the same gate:
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

  function dismiss(id: string): void {
    items.value = items.value.filter((toast) => toast.id !== id);
  }

  /** Show one, unconditionally. The gate is in `arrival` below. */
  function push(toast: Omit<Toast, "id">): string {
    sequence += 1;
    const id = `toast-${sequence}`;
    items.value = [...items.value, { ...toast, id }].slice(-AT_MOST);

    // Timers are per toast rather than one sweep: a stack that all disappears at
    // once reads as a bug, and dismissing one by hand must not shorten the rest.
    setTimeout(() => dismiss(id), LINGER);
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
        channel: event.channel_id,
        priority: event.priority,
        notification: event.notification_id,
        groupKey: event.payload.group_key ?? null,
      })
    ) {
      return;
    }

    push({
      title: event.payload.title,
      body: event.payload.body ?? undefined,
      url: `/n/${event.notification_id}`,
    });
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
   */
  function pushed(envelope: {
    kind?: string;
    title?: string;
    body?: string;
    url?: string;
  }): void {
    if (!envelope.title) {
      return;
    }
    push({ title: envelope.title, body: envelope.body, url: envelope.url });
  }

  return { items, arrival, push, pushed, dismiss };
});
