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

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip } from "@/components/ui/tooltip";
import { exact } from "@/lib/format";
import { lookOf, priorityLabel, stateLabel } from "@/lib/notification";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import ActionBar from "./ActionBar.vue";
import Markdown from "./Markdown.vue";
import RevisionTimeline from "./RevisionTimeline.vue";
import StatusDot from "./StatusDot.vue";

const detail = useDetail();
const channels = useChannels();

const copied = ref(false);

const head = computed(() => detail.notification?.head ?? null);
const image = computed(() => head.value?.content.image ?? null);

/** Receipts are opt-in per channel (§7.3): no policy, no counter. */
const showsReceipts = computed(() =>
  detail.notification
    ? channels.collectsReceipts(detail.notification.channel)
    : false,
);

/** The audience "vu par 3/5" divides by: members who are people (§7.3). */
const audience = computed(() => {
  const channel = detail.notification
    ? channels.byId.get(detail.notification.channel)
    : undefined;
  return (channel?.members ?? []).filter(
    (member) => member.principal.kind === "user",
  ).length;
});

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

// Opening a notification is what `seen` means (§7.3) — but only where the channel
// collects receipts, so a channel that does not is not told who read what.
watch(
  () => [detail.id, showsReceipts.value] as const,
  ([id, collects]) => {
    if (id && collects) {
      void detail.markSeen();
    }
  },
  { immediate: true },
);
</script>

<template>
  <aside
    class="flex h-full min-h-0 flex-col overflow-y-auto"
    aria-label="Détail de la notification"
  >
    <!-- Nothing selected. -->
    <div
      v-if="!detail.id"
      class="flex h-full flex-col items-center justify-center gap-2 p-8 text-center text-sm text-muted-foreground"
    >
      <Link2 class="size-5" />
      <p>Sélectionne une notification pour voir sa timeline.</p>
      <p class="text-xs">↑ ↓ pour naviguer, Entrée pour ouvrir.</p>
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
            ? "Cette notification a expiré."
            : "Cette notification a été supprimée."
        }}
      </p>
      <p class="text-muted-foreground">
        {{
          detail.gone === "expired"
            ? "Sa durée de rétention est écoulée ; le contenu n'est plus conservé (§7.2bis)."
            : "Son auteur l'a retirée. Les clients l'ont retirée de leur inbox."
        }}
      </p>
      <Button variant="outline" size="sm" @click="detail.close()">Fermer</Button>
    </div>

    <div v-else-if="detail.error" class="space-y-2 p-6 text-sm">
      <p class="font-medium text-destructive">{{ detail.error }}</p>
      <Button
        variant="outline"
        size="sm"
        @click="detail.id && detail.open(detail.id)"
      >
        Réessayer
      </Button>
    </div>

    <template v-else-if="head && detail.notification">
      <header class="space-y-2 p-4">
        <div class="flex items-start gap-2">
          <StatusDot :status="head.status" :state="head.state" class="mt-1" />
          <h2 class="flex-1 text-base font-semibold leading-snug">
            {{ head.content.title }}
          </h2>
          <Tooltip>
            <Button variant="ghost" size="icon" @click="copyPermalink">
              <Check v-if="copied" />
              <Copy v-else />
              <span class="sr-only">Copier le permalien</span>
            </Button>
            <template #content>
              {{ copied ? "Copié" : "Copier le permalien /n/…" }}
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
          <Badge variant="muted">priorité {{ priorityLabel(head.priority) }}</Badge>
          <Badge v-if="detail.notification.external_id" variant="muted">
            {{ detail.notification.external_id }}
          </Badge>
          <Tooltip v-if="showsReceipts">
            <Badge variant="muted" class="cursor-default">
              <Eye class="size-3" />
              vu par {{ detail.seenBy }}/{{ audience }}
            </Badge>
            <template #content>
              Les accusés sont comptés en personnes, jamais en devices (§7.3).
            </template>
          </Tooltip>
        </div>

        <p class="text-xs text-muted-foreground">
          Créée le {{ exact(detail.notification.created_at) }}
          <template v-if="detail.notification.closed_at">
            · fermée le {{ exact(detail.notification.closed_at) }}
          </template>
          <template v-if="detail.notification.ttl">
            · rétention {{ detail.notification.ttl }}
          </template>
        </p>
      </header>

      <Separator />

      <section class="space-y-3 p-4">
        <Markdown v-if="head.content.body" :source="head.content.body" />
        <p v-else class="text-sm italic text-muted-foreground">
          Pas de corps — le titre dit tout.
        </p>

        <img
          v-if="image && image.kind === 'data_uri'"
          :src="image.uri"
          alt=""
          class="max-h-64 rounded-md border border-border object-contain"
        />

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
          Timeline · {{ detail.revisions.length }} révision(s)
        </h3>
        <RevisionTimeline
          :revisions="detail.revisions"
          :answers="detail.answers"
        />
      </section>
    </template>
  </aside>
</template>
