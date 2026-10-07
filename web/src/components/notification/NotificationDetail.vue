<script setup lang="ts">
/** The detail panel (§10.4): empty, loading, gone (deleted vs expired, §7.2bis), or the notification. */
import { Check, Copy, Eye } from "@lucide/vue";
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { HoverCard } from "@/components/ui/hover-card";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, exact } from "@/lib/format";
import {
  authorLabel,
  lookOf,
  priorityLabel,
  stateLabel,
} from "@/lib/notification";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import ActionBar from "./ActionBar.vue";
import Markdown from "./Markdown.vue";
import NotificationImage from "./NotificationImage.vue";
import RevisionTimeline from "./RevisionTimeline.vue";
import StatusDot from "./StatusDot.vue";

const detail = useDetail();
const channels = useChannels();
const { t } = useI18n();

const copied = ref(false);

const head = computed(() => detail.notification?.head ?? null);
const image = computed(() => head.value?.content.image ?? null);

/** Receipts are opt-in per channel (§7.3): no policy, no counter. */
const showsReceipts = computed(() =>
  detail.notification
    ? channels.collectsReceipts(detail.notification.channel)
    : false,
);

/** Same principal fallback as the timeline and inbox (§2.4). */
const readers = computed(() =>
  (detail.readership?.seen ?? []).map((sighting, index) => ({
    key: `${authorLabel(sighting.author)}-${index}`,
    label: authorLabel(sighting.author),
    at: sighting.at,
  })),
);

async function copyPermalink(): Promise<void> {
  if (!detail.id) {
    return;
  }
  // Mirrors the deep link (§10.4).
  await navigator.clipboard.writeText(
    `${window.location.origin}/n/${detail.id}`,
  );
  copied.value = true;
  setTimeout(() => {
    copied.value = false;
  }, 2000);
}

// Opening is `seen` (§7.3) on every channel; the server keeps it private where
// receipts are off (D27). Gating here froze the unread badge.
watch(
  () => detail.id,
  (id) => {
    if (id) {
      void detail.markSeen();
    }
  },
  { immediate: true },
);
</script>

<template>
  <aside
    class="flex h-full min-h-0 flex-col overflow-y-auto"
    :aria-label="t('notification.detail.ariaLabel')"
  >
    <!-- Nothing selected. -->
    <div
      v-if="!detail.id"
      class="flex h-full flex-col items-center justify-center gap-3 p-10 text-center"
    >
      <div class="dot-field h-10 w-40" aria-hidden="true" />
      <p class="text-sm text-ink">{{ t("notification.detail.emptyTitle") }}</p>
      <p class="max-w-[40ch] text-xs text-ink-dim">{{ t("notification.detail.emptyHint") }}</p>
    </div>

    <div v-else-if="detail.loading" class="space-y-3 p-5">
      <Skeleton class="h-6 w-2/3" />
      <Skeleton class="h-3 w-1/3" />
      <Skeleton class="h-24 w-full" />
      <Skeleton class="h-3 w-1/2" />
    </div>

    <div v-else-if="detail.gone" class="m-5 space-y-3 border border-rule-strong px-6 py-10 text-sm">
      <p class="pixel text-base font-bold uppercase">
        {{
          detail.gone === "expired"
            ? t("notification.detail.expiredTitle")
            : t("notification.detail.deletedTitle")
        }}
      </p>
      <p class="max-w-[64ch] text-ink-dim">
        {{
          detail.gone === "expired"
            ? t("notification.detail.expiredBody")
            : t("notification.detail.deletedBody")
        }}
      </p>
      <Button variant="outline" size="sm" @click="detail.close()">
        {{ t("notification.detail.close") }}
      </Button>
    </div>

    <div v-else-if="detail.error" class="m-5 space-y-3 border border-signal px-6 py-10 text-sm">
      <p class="text-signal">{{ detail.error }}</p>
      <Button variant="outline" size="sm" @click="detail.id && detail.open(detail.id)">
        {{ t("notification.detail.retry") }}
      </Button>
    </div>

    <template v-else-if="head && detail.notification">
      <header class="space-y-3 px-5 pb-4 pt-5">
        <!-- `pr-12` clears AppLayout's mobile close button. -->
        <div class="flex items-start gap-3 pr-12 xl:pr-0">
          <StatusDot :status="head.status" :state="head.state" class="mt-1.5" />
          <h2 class="flex-1 text-xl leading-snug [overflow-wrap:anywhere]" data-selectable>
            {{ head.content.title }}
          </h2>
          <Tooltip>
            <Button variant="ghost" size="icon" @click="copyPermalink">
              <Check v-if="copied" />
              <Copy v-else />
              <span class="sr-only">{{ t("notification.detail.copyPermalink") }}</span>
            </Button>
            <template #content>
              {{
                copied
                  ? t("notification.detail.copied")
                  : t("notification.detail.copyPermalinkTooltip")
              }}
            </template>
          </Tooltip>
        </div>

        <div class="flex flex-wrap items-center gap-1.5 text-xs">
          <Badge variant="outline" :class="lookOf(head.status).color">
            {{ lookOf(head.status).label }}
          </Badge>
          <Badge variant="outline" :class="head.state === 'closed' ? '' : 'border-copper text-copper'">
            {{ stateLabel(head.state) }}
          </Badge>
          <Badge variant="secondary">
            {{ channels.nameOf(detail.notification.channel) }}
          </Badge>
          <Badge variant="outline">
            {{ t("notification.detail.priorityBadge", { priority: priorityLabel(head.priority) }) }}
          </Badge>
          <Badge v-if="detail.notification.external_id" variant="muted" class="[overflow-wrap:anywhere]">
            {{ detail.notification.external_id }}
          </Badge>
          <HoverCard v-if="showsReceipts">
            <Badge variant="muted" class="cursor-default">
              <Eye class="size-3" />
              {{ t("notification.detail.seenByCount", { count: detail.seenBy }) }}<template
                v-if="detail.audience !== null"
                >/{{ detail.audience }}</template
              >
            </Badge>
            <template #content>
              <div class="space-y-2 text-xs">
                <p class="font-medium">
                  {{ t("notification.detail.seenByHeading", { count: detail.seenBy }) }}
                  <template v-if="detail.audience !== null">
                    {{ t("notification.detail.outOfAudience", { audience: detail.audience }) }}
                  </template>
                </p>
                <!-- IdP-group access has no countable audience (§14.2). -->
                <p v-if="detail.audience === null" class="text-ink-dim">
                  {{ t("notification.detail.noGroupTotal") }}
                </p>

                <ul v-if="readers.length" class="space-y-1">
                  <li
                    v-for="reader in readers"
                    :key="reader.key"
                    class="flex items-baseline justify-between gap-3"
                  >
                    <span class="truncate">{{ reader.label }}</span>
                    <time
                      :datetime="reader.at"
                      :title="exact(reader.at)"
                      class="shrink-0 text-ink-dim"
                    >
                      {{ ago(reader.at) }}
                    </time>
                  </li>
                </ul>
                <p v-else class="text-ink-dim">
                  {{ t("notification.detail.notOpenedYet") }}
                </p>

                <!-- Delivery stays folded: it counts screens, not readers (§10.3). -->
                <p class="border-t border-rule pt-2 text-ink-dim">
                  {{
                    t("notification.detail.deviceCount", {
                      count: detail.readership?.delivered_devices ?? 0,
                    })
                  }}
                </p>
              </div>
            </template>
          </HoverCard>
        </div>

        <p class="text-xs leading-relaxed text-ink-dim">
          {{ t("notification.detail.createdAt", { at: exact(detail.notification.created_at) }) }}
          <template v-if="detail.notification.closed_at">
            {{ t("notification.detail.closedAt", { at: exact(detail.notification.closed_at) }) }}
          </template>
          <template v-if="detail.notification.ttl">
            {{ t("notification.detail.ttl", { ttl: detail.notification.ttl }) }}
          </template>
        </p>
      </header>

      <section class="space-y-4 border-t border-rule px-5 py-5">
        <Markdown v-if="head.content.body" :source="head.content.body" />
        <p v-else class="text-sm text-ink-dim">
          {{ t("notification.detail.noBody") }}
        </p>

        <NotificationImage v-if="image" :image="image" />

        <ActionBar
          v-if="detail.actions.length"
          :actions="detail.actions"
          :answers="detail.answers"
          :acting="detail.acting"
          @act="(interaction, key) => detail.act(interaction, key)"
          @silence="(body) => detail.silence(body)"
        />
      </section>

      <section class="space-y-3 border-t border-rule px-5 py-5">
        <h3 class="meta">
          {{ t("notification.detail.timelineHeading", { count: detail.revisions.length }) }}
        </h3>
        <RevisionTimeline
          :revisions="detail.revisions"
          :answers="detail.answers"
        />
      </section>
    </template>
  </aside>
</template>
