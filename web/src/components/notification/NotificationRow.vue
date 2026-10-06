<script setup lang="ts">
/** One inbox row (§10.3); a `<button>` for keyboard parity with the TUI (§10.1). */
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Badge } from "@/components/ui/badge";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, delay, exact } from "@/lib/format";
import {
  authorLabel,
  isLoud,
  lookOf,
  priorityLabel,
  stateLabel,
} from "@/lib/notification";
import type { Row } from "@/stores/inbox";
import NotificationThumb from "./NotificationThumb.vue";
import StatusDot from "./StatusDot.vue";

const props = defineProps<{
  row: Row;
  channelName: string;
  selected?: boolean;
  /** How many notifications this row stands for, when it heads a group (§10.4). */
  groupSize?: number;
  /** Delivery delay in ms, set only for late stream rows (§7.2). */
  lateBy?: number;
}>();

defineEmits<{ open: [] }>();

const { t } = useI18n();
const head = computed(() => props.row.head);
const tags = computed(() => head.value.content.tags ?? []);
const progress = computed(() => head.value.content.progress ?? null);

/** The word under the matrix: the status once closed, the state before. */
const word = computed(() =>
  head.value.state === "closed"
    ? lookOf(head.value.status).label
    : stateLabel(head.value.state),
);

// The one authored motion: a row resolves when a new revision lands — never on
// first render.
const resolving = ref(false);
watch(
  () => props.row.last_seq,
  () => {
    resolving.value = false;
    requestAnimationFrame(() => {
      resolving.value = true;
    });
  },
);
</script>

<template>
  <button
    type="button"
    :aria-current="selected ? 'true' : undefined"
    :class="[
      'rule-row group flex w-full items-start gap-3 border-l px-4 py-row text-left sm:px-5',
      'hover:bg-ground-raised',
      selected ? 'border-l-signal bg-ground-raised' : 'border-l-transparent',
      resolving && 'resolve',
    ]"
    @click="$emit('open')"
  >
    <StatusDot :status="head.status" :state="head.state" class="mt-1" />

    <!-- §10.3, D30: the fourth surface a notification's image reaches. -->
    <NotificationThumb v-if="head.content.image" :image="head.content.image" />

    <span class="min-w-0 flex-1">
      <span class="flex items-baseline gap-2">
        <span
          :class="[
            'min-w-0 flex-1 truncate text-[0.9375rem] group-hover:underline group-hover:decoration-rule-strong',
            selected ? 'font-bold text-ink' : 'text-ink',
          ]"
        >{{ head.content.title }}</span>
        <Tooltip>
          <span class="hidden shrink-0 text-xs tabular-nums text-ink-dim sm:inline">{{ ago(head.received_at) }}</span>
          <template #content>{{ exact(head.received_at) }}</template>
        </Tooltip>
      </span>

      <span class="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-ink-dim">
        <span :class="['meta', head.state === 'closed' ? lookOf(head.status).color : 'text-copper']">{{ word }}</span>
        <span class="tabular-nums sm:hidden">{{ ago(head.received_at) }}</span>
        <Badge v-if="isLoud(head.priority)" :variant="head.priority === 'urgent' ? 'default' : 'outline'" :class="head.priority === 'high' ? 'border-copper text-copper' : ''">
          {{ priorityLabel(head.priority) }}
        </Badge>
        <span class="truncate">{{ channelName }}</span>
        <span aria-hidden="true">·</span>
        <span class="truncate">{{ authorLabel(head.author) }}</span>
        <span v-if="row.last_seq > 1" class="tabular-nums">
          · {{ t("notification.row.revisionCount", { count: row.last_seq }) }}
        </span>
        <Badge v-if="groupSize && groupSize > 1" variant="secondary">+{{ groupSize - 1 }}</Badge>
        <Badge v-for="tag in tags.slice(0, 3)" :key="tag" variant="muted" class="hidden sm:inline-flex">
          {{ tag }}
        </Badge>

        <!-- §7.2: the two timestamps come from the server, the wording is ours. -->
        <Tooltip v-if="lateBy">
          <Badge variant="outline" class="border-copper text-copper">
            {{ t("notification.row.lateBadge", { delay: delay(lateBy) }) }}
          </Badge>
          <template #content>
            {{ t("notification.row.lateTooltip", { at: exact(head.received_at) }) }}
          </template>
        </Tooltip>
      </span>

      <!-- A running operation says how far it got, when the publisher said so. -->
      <span v-if="progress && head.state !== 'closed'" class="mt-2 flex items-center gap-2">
        <progress
          class="h-1 w-32 appearance-none bg-rule [&::-moz-progress-bar]:bg-copper [&::-webkit-progress-bar]:bg-rule [&::-webkit-progress-value]:bg-copper"
          :value="progress.current"
          :max="progress.total"
        />
        <span class="text-xs tabular-nums text-ink-dim">{{ progress.current }}/{{ progress.total }}</span>
      </span>
    </span>
  </button>
</template>
