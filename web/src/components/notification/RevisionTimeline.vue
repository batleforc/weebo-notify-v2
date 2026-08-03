<script setup lang="ts">
/**
 * The revision timeline (§2.2, §10.3).
 *
 * The point of the panel: not "here is the message" but "here is what changed and
 * when". Each row highlights the diff against the previous revision — `status:
 * info → failure` is what an on-call person opened this to read — and answers are
 * folded into the same chronology, because "who acknowledged, and before or after
 * it failed" is the same question.
 */
import { computed } from "vue";

import type { InteractionRecord } from "@/api/generated/types.gen";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, exact } from "@/lib/format";
import { authorLabel, lookOf, stateLabel } from "@/lib/notification";
import type { TimelineRevision } from "@/stores/detail";
import StatusDot from "./StatusDot.vue";

const props = defineProps<{
  revisions: TimelineRevision[];
  answers: InteractionRecord[];
}>();

interface Entry {
  at: string;
  kind: "revision" | "interaction";
  revision?: TimelineRevision;
  answer?: InteractionRecord;
  /** What this revision changed against the one before it. */
  changes: string[];
}

const entries = computed<Entry[]>(() => {
  const timeline: Entry[] = [];

  props.revisions.forEach((revision, position) => {
    const previous = props.revisions[position - 1];
    const changes: string[] = [];
    if (previous) {
      if (previous.status !== revision.status) {
        changes.push(`status: ${previous.status} → ${revision.status}`);
      }
      if (previous.state !== revision.state) {
        changes.push(
          `état: ${stateLabel(previous.state)} → ${stateLabel(revision.state)}`,
        );
      }
      if (previous.priority !== revision.priority) {
        changes.push(`priorité: ${previous.priority} → ${revision.priority}`);
      }
      if (previous.content.title !== revision.content.title) {
        changes.push("titre modifié");
      }
    }
    timeline.push({
      at: revision.received_at,
      kind: "revision",
      revision,
      changes,
    });
  });

  for (const answer of props.answers) {
    timeline.push({
      at: answer.recorded_at,
      kind: "interaction",
      answer,
      changes: [],
    });
  }

  return timeline.sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
});

function describe(answer: InteractionRecord): string {
  switch (answer.interaction.type) {
    case "ack":
      return `a acquitté (${answer.interaction.key})`;
    case "reply":
      return `a répondu : « ${answer.interaction.text} »`;
    case "choice":
      return `a choisi « ${answer.interaction.option} » pour ${answer.interaction.key}`;
    default:
      return "a répondu";
  }
}
</script>

<template>
  <ol class="relative space-y-3 border-l border-border pl-4">
    <li
      v-for="(entry, position) in entries"
      :key="`${entry.kind}-${entry.revision?.seq ?? entry.answer?.id}-${position}`"
      class="relative"
    >
      <span
        class="absolute -left-[1.3rem] top-0.5 flex size-4 items-center justify-center rounded-full bg-background"
      >
        <StatusDot
          v-if="entry.revision"
          :status="entry.revision.status"
          :state="entry.revision.state"
        />
        <span v-else class="size-2 rounded-full bg-muted-foreground" aria-hidden="true" />
      </span>

      <div class="flex flex-wrap items-baseline gap-x-2 text-sm">
        <template v-if="entry.revision">
          <span class="font-medium">
            Révision {{ entry.revision.seq }}
          </span>
          <span :class="['text-xs', lookOf(entry.revision.status).color]">
            {{ lookOf(entry.revision.status).label }} ·
            {{ stateLabel(entry.revision.state) }}
          </span>
        </template>
        <span v-else-if="entry.answer" class="text-sm">
          <span class="font-medium">{{ authorLabel(entry.answer.author) }}</span>
          {{ describe(entry.answer) }}
        </span>

        <Tooltip>
          <span class="cursor-default text-xs text-muted-foreground">
            {{ ago(entry.at) }}
          </span>
          <template #content>{{ exact(entry.at) }}</template>
        </Tooltip>
      </div>

      <p
        v-if="entry.revision"
        class="text-xs text-muted-foreground"
      >
        par {{ authorLabel(entry.revision.author) }}
        <template v-if="entry.changes.length">
          — <span class="text-foreground">{{ entry.changes.join(", ") }}</span>
        </template>
      </p>
    </li>
  </ol>
</template>
