/**
 * Command palette state (§10.4); matching lives in `lib/palette.ts`. Notifications
 * come from `GET /search` (§8), debounced and last-wins: search is capped at 30/min (§8.2).
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
