/**
 * The command palette's state (design §10.4).
 *
 * # What this store is for, and what it deliberately is not
 *
 * It holds what is typed, what the server answered, and whether the palette is
 * open. The *matching* lives in `lib/palette.ts` as pure functions, because that is
 * the part whose behaviour has to be pinned by tests rather than by opening the app
 * — a ranking is not something looking at a screenshot can check.
 *
 * # Why the server is asked at all
 *
 * Channels and screens are already in memory, so they match instantly. Notifications
 * are not: the inbox holds a page, and §10.4 wants the palette to find a
 * notification the user cannot see. `GET /api/v1/search` is what answers that (§8),
 * and until now nothing in any client called it.
 *
 * The request is **debounced** and **the last one wins**: typing `etcd` would
 * otherwise fire four searches, and the answer to `etc` arriving after the answer to
 * `etcd` would leave the wrong list on screen. §8.2 also caps search at 30/min per
 * principal, which a keystroke-per-request palette would burn through in twenty
 * seconds.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { search as searchNotifications } from "@/api/generated";
import type { Notification } from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import {
  channelItems,
  notificationItems,
  type PaletteItem,
  rank,
  staticItems,
} from "@/lib/palette";
import { useChannels } from "./channels";

/** How long a keystroke waits before it becomes a request. */
export const DEBOUNCE_MS = 200;

/** Below this, only what is already in memory is searched. */
const MIN_QUERY = 2;

export const usePalette = defineStore("palette", () => {
  const open = ref(false);
  const query = ref("");
  const results = ref<Notification[]>([]);
  const searching = ref(false);
  const error = ref<string | null>(null);

  const channels = useChannels();

  /** Every offered row, best first, grouped as §10.4 asks. */
  const items = computed<PaletteItem[]>(() => {
    const local = rank(
      [...channelItems(channels.all), ...staticItems()],
      query.value,
    );
    // Server results are not re-ranked: the server ordered them by relevance and
    // recency, and a second opinion computed from the title alone would be worse.
    return [...local, ...notificationItems(results.value)];
  });

  let pending: ReturnType<typeof setTimeout> | null = null;
  /** Which request is the newest. An older answer that lands late is dropped. */
  let generation = 0;

  function reset(): void {
    // Bumped so an answer already in flight is dropped when it lands: closing the
    // palette must not repopulate it a second later.
    generation += 1;
    query.value = "";
    results.value = [];
    error.value = null;
    searching.value = false;
    if (pending) {
      clearTimeout(pending);
      pending = null;
    }
  }

  function show(): void {
    reset();
    open.value = true;
  }

  function hide(): void {
    open.value = false;
    reset();
  }

  function toggle(): void {
    if (open.value) {
      hide();
    } else {
      show();
    }
  }

  /** Run the search now, skipping the debounce — what a test and `Enter` want. */
  async function searchNow(text: string): Promise<void> {
    const mine = ++generation;
    if (text.trim().length < MIN_QUERY) {
      results.value = [];
      searching.value = false;
      return;
    }

    searching.value = true;
    error.value = null;
    try {
      const page = await searchNotifications({
        query: { q: text, limit: 8 },
        throwOnError: true,
      });
      if (mine !== generation) {
        // A newer keystroke has already been answered; this one is stale.
        return;
      }
      results.value = page.data.items;
    } catch (cause) {
      if (mine !== generation) {
        return;
      }
      // Shown on the palette rather than thrown: a search that fails must not take
      // the switcher down with it — the channels and screens still match locally.
      error.value = failureOf(cause).message;
      results.value = [];
    } finally {
      if (mine === generation) {
        searching.value = false;
      }
    }
  }

  /** What the input calls on every keystroke. */
  function type(text: string): void {
    query.value = text;
    if (pending) {
      clearTimeout(pending);
    }
    pending = setTimeout(() => {
      pending = null;
      void searchNow(text);
    }, DEBOUNCE_MS);
  }

  return {
    open,
    query,
    results,
    searching,
    error,
    items,
    show,
    hide,
    toggle,
    type,
    searchNow,
  };
});
