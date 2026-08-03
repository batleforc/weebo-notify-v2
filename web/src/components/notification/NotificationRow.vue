<script setup lang="ts">
/**
 * One row of the inbox (§10.3).
 *
 * The row is a `<button>` rather than a `<div>` with a click handler, which is
 * what gets it keyboard focus, Enter and Space for free — the inbox has to be
 * drivable without a mouse to match the TUI (§10.1).
 */
import { computed } from "vue";
import { Badge } from "@/components/ui/badge";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, delay, exact } from "@/lib/format";
import {
  accentOf,
  authorLabel,
  isLoud,
  priorityLabel,
} from "@/lib/notification";
import type { Row } from "@/stores/inbox";
import StatusDot from "./StatusDot.vue";

const props = defineProps<{
  row: Row;
  channelName: string;
  selected?: boolean;
  /** How many notifications this row stands for, when it heads a group (§10.4). */
  groupSize?: number;
  /**
   * How late this row reached us, in milliseconds — set only for rows the stream
   * replayed after a reconnect (§7.2).
   */
  lateBy?: number;
}>();

defineEmits<{ open: [] }>();

const head = computed(() => props.row.head);
const tags = computed(() => head.value.content.tags ?? []);
const progress = computed(() => head.value.content.progress ?? null);
</script>

<template>
  <button
    type="button"
    :aria-current="selected ? 'true' : undefined"
    :class="[
      'group flex w-full items-start gap-3 border-l-2 px-3 py-row text-left transition-colors',
      'hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:outline-none',
      accentOf(head.priority),
      selected ? 'bg-accent' : 'bg-transparent',
    ]"
    @click="$emit('open')"
  >
    <StatusDot :status="head.status" :state="head.state" class="mt-0.5 shrink-0" />

    <span class="min-w-0 flex-1">
      <span class="flex items-baseline gap-2">
        <span class="truncate text-sm font-medium">{{ head.content.title }}</span>
        <Badge v-if="groupSize && groupSize > 1" variant="muted" class="shrink-0">
          +{{ groupSize - 1 }}
        </Badge>
        <Badge v-if="isLoud(head.priority)" variant="outline" class="shrink-0">
          {{ priorityLabel(head.priority) }}
        </Badge>
      </span>

      <span class="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
        <span class="font-medium">{{ channelName }}</span>
        <span aria-hidden="true">·</span>
        <span>{{ authorLabel(head.author) }}</span>
        <Tooltip>
          <span class="cursor-default">{{ ago(head.received_at) }}</span>
          <template #content>{{ exact(head.received_at) }}</template>
        </Tooltip>
        <span v-if="row.last_seq > 1" class="tabular-nums">
          · {{ row.last_seq }} révisions
        </span>
        <Badge
          v-for="tag in tags.slice(0, 3)"
          :key="tag"
          variant="muted"
          class="font-normal"
        >
          {{ tag }}
        </Badge>

        <!-- §7.2: the two timestamps come from the server, the wording is ours. -->
        <Tooltip v-if="lateBy">
          <Badge variant="outline" class="border-status-warning text-status-warning">
            reçue en retard ({{ delay(lateBy) }})
          </Badge>
          <template #content>
            Publiée le {{ exact(head.received_at) }}, reçue par ce client à
            l'instant.
          </template>
        </Tooltip>
      </span>

      <!-- A running operation says how far it got, when the publisher said so. -->
      <span
        v-if="progress && head.state !== 'closed'"
        class="mt-1.5 flex items-center gap-2"
      >
        <progress
          class="h-1 w-32 overflow-hidden rounded-full [&::-moz-progress-bar]:bg-primary [&::-webkit-progress-bar]:bg-muted [&::-webkit-progress-value]:bg-primary"
          :value="progress.current"
          :max="progress.total"
        />
        <span class="text-xs tabular-nums text-muted-foreground">
          {{ progress.current }}/{{ progress.total }}
        </span>
      </span>
    </span>
  </button>
</template>
