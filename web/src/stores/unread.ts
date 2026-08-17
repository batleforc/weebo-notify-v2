/**
 * The unread badge, per channel (§7.3, §9.2, §10.4).
 *
 * The count is the **server's**, fetched from `GET /unread`, and never derived from
 * the rows this tab happens to hold. That distinction is the whole of D27: a badge
 * folded over the loaded page would mean "unread among the fifty rows I have", a
 * different number that looks exactly like the right one and is wrong for anybody
 * whose channel is bigger than a page.
 *
 * Channels with nothing unread are absent from the answer rather than present with
 * a zero, so "no badge" has one representation here too — `countOf` turns the
 * absence into the zero a template wants without ever storing it.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { unread as fetchUnread } from "@/api/generated";
import { failureOf } from "@/api/problem";

/**
 * How long a burst of stream events waits before it becomes one request.
 *
 * A badge changes on every publish and every read, so a deploy fanning out forty
 * revisions (§10.4's "et 39 autres") would be forty round trips for a number that
 * is only ever rendered once per frame. The trailing edge is the right one: what
 * matters is the count *after* the burst, not during it.
 */
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
