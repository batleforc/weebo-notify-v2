/**
 * Local popups (§7.4). Stream arrivals and visible-tab pushes both pass the same
 * gate; this only decides what is *shown*, the inbox still keeps everything.
 */

import { defineStore } from "pinia";
import { ref } from "vue";

import type { Event as StreamEvent } from "@/api/generated";
import { usePreferences } from "@/stores/preferences";

const LINGER = 6_000;

const AT_MOST = 4;

/** One toast per notification per window: stream and push may both announce it. */
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

  /** Pause while hovered/focused (WCAG 2.2.1). */
  function hold(id: string): void {
    clearTimeout(timers.get(id));
    timers.delete(id);
  }

  function release(id: string): void {
    if (!timers.has(id) && items.value.some((toast) => toast.id === id)) {
      timers.set(
        id,
        setTimeout(() => dismiss(id), LINGER),
      );
    }
  }

  /** Show one, unless this notification just had a toast. */
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

  /** Only revisions raise toasts; receipts and interactions would flood busy channels. */
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
   * A push forwarded by the worker for a visible tab. DND was applied server-side
   * (D22); silences are checked here. Pokes (no title, §6.3) raise nothing.
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
