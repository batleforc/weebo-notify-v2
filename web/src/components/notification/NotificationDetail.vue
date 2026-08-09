<script setup lang="ts">
/**
 * The detail panel (§10.4): what it says, what changed, what was answered.
 *
 * States it has to get right, and each one is a different sentence:
 *   * nothing selected — an invitation, not an error;
 *   * loading — skeletons that match the shape about to appear;
 *   * deleted or expired — the notification is gone and the panel says which,
 *     because §7.2bis words those two differently on purpose;
 *   * refused — the problem's `code` decided the message (§8).
 */
import { Check, Copy, Eye, Link2 } from "lucide-vue-next";
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { HoverCard } from "@/components/ui/hover-card";
import { Separator } from "@/components/ui/separator";
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

/**
 * Receipts are opt-in per channel (§7.3): no policy, no counter.
 *
 * Read from the channel policy rather than from the readership being present,
 * because this also gates the `seen` receipt this panel posts — asking the channel
 * first saves a request the server would answer with nothing.
 */
const showsReceipts = computed(() =>
  detail.notification
    ? channels.collectsReceipts(detail.notification.channel)
    : false,
);

/**
 * Who read it, in the words the rest of the app uses for a principal (§2.4).
 *
 * `authorLabel` rather than the raw `display_name`: a profile the server could not
 * name still has to render as something, and it is the same fallback the timeline
 * and the inbox already show for the same person.
 */
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
  // §10.4: the permalink mirrors the deep link exactly, so what lands in a chat
  // message is the same address a desktop client will one day open.
  await navigator.clipboard.writeText(
    `${window.location.origin}/n/${detail.id}`,
  );
  copied.value = true;
  setTimeout(() => {
    copied.value = false;
  }, 2000);
}

// Opening a notification is what `seen` means (§7.3), and it is posted on **every**
// channel — including the ones that collect no receipts.
//
// It used to be gated on `showsReceipts`, on the reasoning that a channel opting out
// of receipts should not be told who read what. That is still true, and it is the
// server that enforces it: a `seen` on such a channel is recorded and broadcast only
// to the reader's own connections, never to the channel (D27). Gating it here as
// well meant the unread badge never dropped on a default channel — `receipts_enabled`
// is false unless an operator says otherwise — so the badge sat at the channel's
// full size for ever.
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
      class="flex h-full flex-col items-center justify-center gap-2 p-8 text-center text-sm text-muted-foreground"
    >
      <Link2 class="size-5" />
      <p>{{ t("notification.detail.emptyTitle") }}</p>
      <p class="text-xs">{{ t("notification.detail.emptyHint") }}</p>
    </div>

    <div v-else-if="detail.loading" class="space-y-3 p-4">
      <Skeleton class="h-5 w-2/3" />
      <Skeleton class="h-3 w-1/3" />
      <Skeleton class="h-20 w-full" />
      <Skeleton class="h-3 w-1/2" />
    </div>

    <div v-else-if="detail.gone" class="space-y-2 p-6 text-sm">
      <p class="font-medium">
        {{
          detail.gone === "expired"
            ? t("notification.detail.expiredTitle")
            : t("notification.detail.deletedTitle")
        }}
      </p>
      <p class="text-muted-foreground">
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

    <div v-else-if="detail.error" class="space-y-2 p-6 text-sm">
      <p class="font-medium text-destructive">{{ detail.error }}</p>
      <Button
        variant="outline"
        size="sm"
        @click="detail.id && detail.open(detail.id)"
      >
        {{ t("notification.detail.retry") }}
      </Button>
    </div>

    <template v-else-if="head && detail.notification">
      <header class="space-y-2 p-4">
        <!-- `pr-10` clears `AppLayout`'s absolutely-positioned mobile close button
             (`right-2 top-2`, `xl:hidden`): without it, this row's own trailing
             button sits in the exact same corner under `xl`. -->
        <div class="flex items-start gap-2 pr-10 xl:pr-0">
          <StatusDot :status="head.status" :state="head.state" class="mt-1" />
          <h2 class="flex-1 text-base font-semibold leading-snug">
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
          <Badge variant="secondary">
            {{ channels.nameOf(detail.notification.channel) }}
          </Badge>
          <Badge variant="outline" :class="lookOf(head.status).color">
            {{ lookOf(head.status).label }}
          </Badge>
          <Badge variant="muted">{{ stateLabel(head.state) }}</Badge>
          <Badge variant="muted">
            {{ t("notification.detail.priorityBadge", { priority: priorityLabel(head.priority) }) }}
          </Badge>
          <Badge v-if="detail.notification.external_id" variant="muted">
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
                <!--
                  §14.2: l'accès par groupe OIDC n'est jamais matérialisé, donc
                  personne ici ne sait combien de gens sont dans le groupe. Un
                  total inventé à partir des membres stockés dirait « tout le
                  monde l'a vue » sur un canal que toute une équipe peut lire.
                -->
                <p v-if="detail.audience === null" class="text-muted-foreground">
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
                      class="shrink-0 text-muted-foreground"
                    >
                      {{ ago(reader.at) }}
                    </time>
                  </li>
                </ul>
                <p v-else class="text-muted-foreground">
                  {{ t("notification.detail.notOpenedYet") }}
                </p>

                <!-- Delivery stays folded: it counts screens, not readers (§10.3). -->
                <p class="border-t border-border pt-2 text-muted-foreground">
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

        <p class="text-xs text-muted-foreground">
          {{ t("notification.detail.createdAt", { at: exact(detail.notification.created_at) }) }}
          <template v-if="detail.notification.closed_at">
            {{ t("notification.detail.closedAt", { at: exact(detail.notification.closed_at) }) }}
          </template>
          <template v-if="detail.notification.ttl">
            {{ t("notification.detail.ttl", { ttl: detail.notification.ttl }) }}
          </template>
        </p>
      </header>

      <Separator />

      <section class="space-y-3 p-4">
        <Markdown v-if="head.content.body" :source="head.content.body" />
        <p v-else class="text-sm italic text-muted-foreground">
          {{ t("notification.detail.noBody") }}
        </p>

        <!--
          Both kinds, since M4quatervicies. The `kind === 'data_uri'` guard that
          used to be here was the whole of D26: an attachment reference — what the
          CLI produces above 48 KiB — matched nothing and rendered nothing.
        -->
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

      <Separator />

      <section class="space-y-3 p-4">
        <h3 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
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
