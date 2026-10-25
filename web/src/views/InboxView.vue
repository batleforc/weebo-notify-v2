<script setup lang="ts">
/**
 * The inbox list (§10.4). Arrivals wait behind a "N new" separator (§10.5); updates
 * to visible rows apply at once. Keyboard: ↑ ↓ move, Enter opens, Esc closes (§10.1).
 */
import { Inbox as InboxIcon, RefreshCw } from "@lucide/vue";
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";

import NotificationRow from "@/components/notification/NotificationRow.vue";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { listStep } from "@/lib/list-keys";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import { useInbox } from "@/stores/inbox";

const channels = useChannels();
const detail = useDetail();
const inbox = useInbox();
const route = useRoute();
const router = useRouter();
const { t } = useI18n();

/** The empty state reads differently when filtered (§10.4). */
const isFiltered = computed(
  () =>
    inbox.filters.channels.length > 0 ||
    inbox.filters.state.length > 0 ||
    inbox.filters.status.length > 0 ||
    inbox.filters.tags.length > 0 ||
    inbox.filters.minPriority !== null ||
    inbox.filters.hasActions !== null,
);

const list = ref<HTMLElement | null>(null);
const cursor = ref(0);

const groups = computed(() => inbox.groups);
const liveCount = computed(
  () => groups.value.filter((g) => g.head.head.state !== "closed").length,
);
const title = computed(() => {
  const channel = route.params.channel;
  return typeof channel === "string"
    ? channels.nameOf(channel)
    : t("sidebar.channels.all");
});

/** Lateness (§7.2), only for rows delivered after this view opened: a cold page isn't "late". */
const LATE_THRESHOLD_MS = 5 * 60_000;
const openedAt = Date.now();

function lateness(receivedAt: string): number | undefined {
  const at = Date.parse(receivedAt);
  if (Number.isNaN(at) || at < openedAt) {
    return undefined;
  }
  const late = Date.now() - at;
  return late > LATE_THRESHOLD_MS ? late : undefined;
}

function open(id: string, viaKeyboard = false): void {
  // Inside a channel, open within it so the URL keeps naming the channel.
  const channel = route.params.channel;
  const target =
    typeof channel === "string"
      ? { name: "channel-notification", params: { channel, id } }
      : { name: "notification", params: { id } };
  // Arrow keys replace, so Back leaves the list instead of walking it.
  void (viaKeyboard ? router.replace(target) : router.push(target));
}

function move(delta: number): void {
  const rows = groups.value;
  if (!rows.length) {
    return;
  }
  cursor.value = Math.min(Math.max(cursor.value + delta, 0), rows.length - 1);
  const row = rows[cursor.value];
  if (!row) {
    return;
  }
  open(row.head.id, true);

  const element = list.value?.querySelector<HTMLElement>(
    `[data-row="${row.head.id}"]`,
  );
  element?.scrollIntoView({ block: "nearest" });
  element?.focus({ preventScroll: true });
}

function onKeydown(event: KeyboardEvent): void {
  if (event.target instanceof HTMLInputElement) {
    return;
  }
  const step = listStep(event);
  if (step) {
    event.preventDefault();
    move(step);
  } else if (event.key === "Escape" && detail.id) {
    event.preventDefault();
    void router.push({ name: "inbox" });
  }
}

onMounted(() => window.addEventListener("keydown", onKeydown));
onBeforeUnmount(() => window.removeEventListener("keydown", onKeydown));
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <header class="flex items-end justify-between gap-3 border-b border-rule px-4 pb-3 pt-4 sm:px-5">
      <div class="min-w-0">
        <h1 class="pixel truncate text-2xl font-bold uppercase">{{ title }}</h1>
        <p class="meta mt-1 tabular-nums">
          {{ t("inbox.summary", { total: groups.length, live: liveCount }) }}
        </p>
      </div>
      <span v-if="isFiltered" class="meta shrink-0 text-copper">{{ t("inbox.filtered") }}</span>
    </header>

    <!-- §10.5: the separator is how an arrival announces itself. -->
    <button
      v-if="inbox.pendingCount && !inbox.loading"
      type="button"
      class="meta flex items-center justify-center gap-2 border-b border-rule-strong bg-ground-raised py-2 text-ink hover:bg-ground-sunk"
      @click="inbox.revealPending()"
    >
      <RefreshCw class="size-3.5" />
      {{ t("inbox.newNotifications", { count: inbox.pendingCount }, inbox.pendingCount) }}
    </button>

    <div ref="list" class="min-h-0 flex-1 overflow-y-auto overscroll-contain">
      <div v-if="inbox.loading" class="space-y-px">
        <Skeleton v-for="index in 6" :key="index" class="h-16 w-full" />
      </div>

      <div v-else-if="inbox.error" class="m-4 border border-signal px-6 py-10 text-center sm:m-5">
        <p class="pixel text-base font-bold uppercase text-signal">{{ t("inbox.errorTitle") }}</p>
        <p class="mx-auto mt-2 max-w-[64ch] text-sm text-ink-dim">{{ inbox.error }}</p>
        <Button variant="outline" size="sm" class="mt-4" @click="inbox.load()">
          {{ t("inbox.retry") }}
        </Button>
      </div>

      <!-- An empty inbox is not an error, and neither is a filter that matches
           nothing — §10.4 asks for those to read differently. -->
      <div v-else-if="!groups.length" class="m-4 border border-rule-strong px-6 py-10 text-center sm:m-5">
        <InboxIcon class="mx-auto size-6 text-ink-dim" />
        <template v-if="isFiltered">
          <p class="pixel mt-3 text-base font-bold uppercase">{{ t("inbox.emptyFiltered.title") }}</p>
          <p class="mx-auto mt-2 max-w-[64ch] text-sm text-ink-dim">{{ t("inbox.emptyFiltered.hint") }}</p>
        </template>
        <template v-else>
          <p class="pixel mt-3 text-base font-bold uppercase">{{ t("inbox.emptyDefault.title") }}</p>
          <p class="mx-auto mt-2 max-w-[64ch] text-sm text-ink-dim">{{ t("inbox.emptyDefault.hint") }}</p>
        </template>
      </div>

      <ul v-else>
        <li v-for="group in groups" :key="group.head.id" :data-row="group.head.id">
          <NotificationRow
            :row="group.head"
            :channel-name="channels.nameOf(group.head.channel)"
            :selected="detail.id === group.head.id"
            :group-size="group.members.length"
            :late-by="lateness(group.head.head.received_at)"
            @open="open(group.head.id)"
          />
        </li>
      </ul>

      <div v-if="inbox.hasMore" class="p-4">
        <Button
          variant="outline"
          size="sm"
          class="w-full"
          :disabled="inbox.loadingMore"
          @click="inbox.loadMore()"
        >
          {{ inbox.loadingMore ? t("inbox.loading") : t("inbox.loadMore") }}
        </Button>
      </div>
    </div>
  </div>
</template>
