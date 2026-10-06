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

const entries = computed<Entry[]>(() => {
  const timeline: Entry[] = [];

  props.revisions.forEach((revision, position) => {
    const previous = props.revisions[position - 1];
    const changes: string[] = [];
    if (previous) {
      if (previous.status !== revision.status) {
        changes.push(
          t("notification.revisionTimeline.statusChanged", {
            from: lookOf(previous.status).label,
            to: lookOf(revision.status).label,
          }),
        );
      }
      if (previous.state !== revision.state) {
        changes.push(
          t("notification.revisionTimeline.stateChanged", {
            from: stateLabel(previous.state),
            to: stateLabel(revision.state),
          }),
        );
      }
      if (previous.priority !== revision.priority) {
        changes.push(
          t("notification.revisionTimeline.priorityChanged", {
            from: priorityLabel(previous.priority),
            to: priorityLabel(revision.priority),
          }),
        );
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
