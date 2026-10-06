<script setup lang="ts">
/** The revision timeline (§2.2, §10.3): each row's diff against the previous one, with answers interleaved. */
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import type { InteractionRecord } from "@/api/generated/types.gen";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, exact } from "@/lib/format";
import {
  authorLabel,
  lookOf,
  priorityLabel,
  stateLabel,
} from "@/lib/notification";
import type { TimelineRevision } from "@/stores/detail";
import StatusDot from "./StatusDot.vue";

const props = defineProps<{
  revisions: TimelineRevision[];
  answers: InteractionRecord[];
}>();

const { t } = useI18n();

interface Entry {
  at: string;
  kind: "revision" | "interaction";
  revision?: TimelineRevision;
  answer?: InteractionRecord;
  /** What this revision changed against the one before it. */
  changes: string[];
}

const DIFFS: [
  "status" | "state" | "priority",
  string,
  (r: TimelineRevision) => string,
][] = [
  ["status", "statusChanged", (r) => lookOf(r.status).label],
  ["state", "stateChanged", (r) => stateLabel(r.state)],
  ["priority", "priorityChanged", (r) => priorityLabel(r.priority)],
];

const entries = computed<Entry[]>(() => {
  const timeline: Entry[] = [];

  props.revisions.forEach((revision, position) => {
    const previous = props.revisions[position - 1];
    const changes: string[] = [];
    if (previous) {
      for (const [field, key, label] of DIFFS) {
        if (previous[field] !== revision[field]) {
          changes.push(
            t(`notification.revisionTimeline.${key}`, {
              from: label(previous),
              to: label(revision),
            }),
          );
        }
      }
      if (previous.content.title !== revision.content.title) {
        changes.push(t("notification.revisionTimeline.titleChanged"));
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
      return t("notification.revisionTimeline.acked", {
        key: answer.interaction.key,
      });
    case "reply":
      return t("notification.revisionTimeline.replied", {
        text: answer.interaction.text,
      });
    case "choice":
      return t("notification.revisionTimeline.chose", {
        option: answer.interaction.option,
        key: answer.interaction.key,
      });
    default:
      return t("notification.revisionTimeline.respondedGeneric");
  }
}
</script>

<template>
  <ol class="relative space-y-4 border-l border-dashed border-rule pl-5">
    <li
      v-for="(entry, position) in entries"
      :key="`${entry.kind}-${entry.revision?.seq ?? entry.answer?.id}-${position}`"
      class="relative"
    >
      <span
        class="absolute -left-[1.6rem] top-0.5 flex size-4 items-center justify-center bg-background"
      >
        <StatusDot
          v-if="entry.revision"
          :status="entry.revision.status"
          :state="entry.revision.state"
        />
        <span v-else class="size-1.5 bg-ink-dim" aria-hidden="true" />
      </span>

      <div class="flex flex-wrap items-baseline gap-x-2 text-sm">
        <template v-if="entry.revision">
          <span class="font-medium">
            {{ t("notification.revisionTimeline.revisionHeading", { seq: entry.revision.seq }) }}
          </span>
          <span :class="['meta', lookOf(entry.revision.status).color]">
            {{ lookOf(entry.revision.status).label }} ·
            {{ stateLabel(entry.revision.state) }}
          </span>
        </template>
        <span v-else-if="entry.answer" class="text-sm">
          <span class="font-medium">{{ authorLabel(entry.answer.author) }}</span>
          {{ describe(entry.answer) }}
        </span>

        <Tooltip>
          <span class="cursor-default text-xs text-ink-dim">
            {{ ago(entry.at) }}
          </span>
          <template #content>{{ exact(entry.at) }}</template>
        </Tooltip>
      </div>

      <p
        v-if="entry.revision"
        class="text-xs text-ink-dim"
      >
        {{ t("notification.revisionTimeline.authoredBy", { author: authorLabel(entry.revision.author) }) }}
        <template v-if="entry.changes.length">
          — <span class="text-ink">{{ entry.changes.join(", ") }}</span>
        </template>
      </p>
    </li>
  </ol>
</template>
