/** The channels the principal can see (§8). The server decides visibility (§14.2). */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import { listChannels } from "@/api/generated";
import type { ChannelView } from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";

export const useChannels = defineStore("channels", () => {
  const all = ref<ChannelView[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

  const byId = computed(
    () => new Map(all.value.map((channel) => [channel.id, channel])),
  );

  /** The label to show: the display name, falling back to the id (§8). */
  function nameOf(id: string): string {
    return byId.value.get(id)?.display_name || id;
  }

  /** Whether receipts are collected here — the detail panel hides them if not. */
  function collectsReceipts(id: string): boolean {
    return byId.value.get(id)?.policy.receipts_enabled ?? false;
  }

  /** Whether the caller may close notifications here by hand (RFC 0004): a convenience, the server decides. */
  function mayClose(id: string): boolean {
    const scopes = byId.value.get(id)?.my_scopes ?? [];
    return scopes.includes("publish") || scopes.includes("manage");
  }

  async function load(): Promise<void> {
    loading.value = true;
    error.value = null;
    const collected: ChannelView[] = [];
    try {
      // A cursor is followed until it comes back `null`, never until a page looks
      // short: rows can expire between two calls (§8).
      let cursor: string | undefined;
      do {
        const page = await listChannels({
          query: { cursor, limit: 100 },
          throwOnError: true,
        });
        collected.push(...page.data.items);
        cursor = page.data.next ?? undefined;
      } while (cursor);

      all.value = collected.sort((a, b) =>
        (a.display_name || a.id).localeCompare(b.display_name || b.id),
      );
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      loading.value = false;
    }
  }

  return {
    all,
    byId,
    collectsReceipts,
    error,
    load,
    loading,
    mayClose,
    nameOf,
  };
});
