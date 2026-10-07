/**
 * Unread badges per channel (§7.3, §10.4): the server's count from `GET /unread`,
 * never derived from loaded rows (D27). Zero channels are absent from the answer.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { unread as fetchUnread } from "@/api/generated";
import { failureOf } from "@/api/problem";

/** Trailing-edge debounce: a burst of events becomes one request. */
export const COALESCE_MS = 500;

export const useUnread = defineStore("unread", () => {
  const counts = ref<Record<string, number>>({});
  const error = ref<string | null>(null);

  /** What the sidebar shows next to one channel. Absent means nothing unread. */
  function countOf(channel: string): number {
    return counts.value[channel] ?? 0;
  }

  /** What a tray or a dock badge shows: one number for everything (§10bis). */
  const total = computed(() =>
    Object.values(counts.value).reduce((sum, count) => sum + count, 0),
  );

  async function load(): Promise<void> {
    error.value = null;
    try {
      const answer = await fetchUnread({ throwOnError: true });
      const next: Record<string, number> = {};
      for (const entry of answer.data) {
        next[entry.channel] = entry.unread;
      }
      // Replaced wholesale, never merged: a channel that dropped out of the answer
      // has nothing unread, and merging would leave its badge lit forever.
      counts.value = next;
    } catch (caught) {
      error.value = failureOf(caught).message;
    }
  }

  let pending: ReturnType<typeof setTimeout> | null = null;

  /** Ask again, once, after the current burst of events settles. */
  function refresh(): void {
    if (pending) {
      clearTimeout(pending);
    }
    pending = setTimeout(() => {
      pending = null;
      void load();
    }, COALESCE_MS);
  }

  /** Drop the timer, so a torn-down layout does not fire one more request. */
  function stop(): void {
    if (pending) {
      clearTimeout(pending);
      pending = null;
    }
  }

  return { counts, countOf, error, load, refresh, stop, total };
});
