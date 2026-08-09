<script setup lang="ts">
/**
 * The middle zone: the inbox itself (§10.4).
 *
 * # Arrivals are announced, not inserted
 *
 * §10.5 forbids the list moving under a cursor. New notifications land in the
 * store's `pending` buffer and this view shows a "N nouvelles" separator; the list
 * only grows when somebody asks it to. Updates to rows already on screen apply
 * immediately, because that is the revision model doing its job — a pipeline going
 * from `working` to `failure` must not wait for a click.
 *
 * # Keyboard first
 *
 * ↑ ↓ move, Entrée opens, Échap closes. Parity with the TUI is a stated goal
 * (§10.1), and it is also what makes the list usable while reading a stack trace
 * with the other hand on a terminal.
 */
import { Inbox as InboxIcon, RefreshCw } from "lucide-vue-next";
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import NotificationRow from "@/components/notification/NotificationRow.vue";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import { useInbox } from "@/stores/inbox";

const channels = useChannels();
const detail = useDetail();
const inbox = useInbox();
const router = useRouter();
const { t } = useI18n();

/** Whether any filter narrows the inbox — the empty state reads differently
 * when it does (§10.4). */
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

/**
 * How late a row reached this client (§7.2).
 *
 * Only rows the stream delivered are measured: a page of `GET /notifications` is
 * "what exists", not "what was delivered to me", and pretending otherwise would
 * badge every notification older than the threshold on a cold load. The complete
 * version needs the per-device sync cursor, which is M2's `GET /sync`.
 */
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
  const target = { name: "notification", params: { id } };
  // Arrow keys REPLACE rather than push: holding a key through fifty rows would
  // otherwise leave fifty history entries, and Back would walk the list backwards
  // one row at a time instead of leaving it. A click is a real navigation and
  // keeps its entry.
  void (viaKeyboard ? router.replace(target) : router.push(target));
}

function move(delta: number): void {
  const rows = groups.value;
  if (!rows.length) {
    return;
  }
  cursor.value = Math.min(Math.max(cursor.value + delta, 0), rows.length - 1);
  const row = rows[cursor.value];
  open(row.head.id, true);

  // Keep the focused row in view without yanking the whole list around.
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
  if (event.key === "ArrowDown" || event.key === "j") {
    event.preventDefault();
    move(1);
  } else if (event.key === "ArrowUp" || event.key === "k") {
    event.preventDefault();
    move(-1);
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
    <!-- §10.5: the separator is how an arrival announces itself. -->
    <button
      v-if="inbox.pendingCount"
      type="button"
      class="flex items-center justify-center gap-2 border-b border-border bg-accent/50 py-2 text-xs font-medium transition-colors hover:bg-accent"
      @click="inbox.revealPending()"
    >
      <RefreshCw class="size-3.5" />
      {{ t("inbox.newNotifications", { count: inbox.pendingCount }, inbox.pendingCount) }}
    </button>

    <div ref="list" class="min-h-0 flex-1 overflow-y-auto">
      <div v-if="inbox.loading" class="space-y-1 p-3">
        <Skeleton v-for="index in 6" :key="index" class="h-12 w-full" />
      </div>

      <div
        v-else-if="inbox.error"
        class="flex flex-col items-center gap-3 p-10 text-center text-sm"
      >
        <p class="text-destructive">{{ inbox.error }}</p>
        <Button variant="outline" size="sm" @click="inbox.load()">
          {{ t("inbox.retry") }}
        </Button>
      </div>

      <!-- An empty inbox is not an error, and neither is a filter that matches
           nothing — §10.4 asks for those to read differently. -->
      <div
        v-else-if="!groups.length"
        class="flex flex-col items-center gap-2 p-10 text-center text-sm text-muted-foreground"
      >
        <InboxIcon class="size-6" />
        <template v-if="isFiltered">
          <p>{{ t("inbox.emptyFiltered.title") }}</p>
          <p class="text-xs">{{ t("inbox.emptyFiltered.hint") }}</p>
        </template>
        <template v-else>
          <p>{{ t("inbox.emptyDefault.title") }}</p>
          <p class="text-xs">
            {{ t("inbox.emptyDefault.hint") }}
          </p>
        </template>
      </div>

      <ul v-else class="divide-y divide-border">
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

      <div v-if="inbox.hasMore" class="p-3">
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
