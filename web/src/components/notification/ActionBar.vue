<script setup lang="ts">
/**
 * The buttons at the foot of a notification (§2.3, §10.3).
 *
 * Order is display priority and comes from the server — the array is rendered as
 * it arrives, never re-sorted, because the publisher decided what matters.
 *
 * Each kind gets the variant §10.3 assigns it, and each one means something
 * different on the wire: `view` is client-side only and produces no interaction,
 * `ack` and `choice` are keyed and idempotent, `reply` is free text, and `silence`
 * is a preference rather than an answer.
 */
import { BellOff, Check, ExternalLink, Send } from "@lucide/vue";
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import type {
  Action,
  Interaction,
  InteractionRecord,
  SilenceBody,
} from "@/api/generated/types.gen";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuItem } from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { isWebUrl } from "@/lib/markdown";

const props = defineProps<{
  actions: Action[];
  /** Interactions already recorded, so an answered action shows as answered. */
  answers: InteractionRecord[];
  /** The key currently in flight — exactly one button spins. */
  acting: string | null;
}>();

const emit = defineEmits<{
  act: [interaction: Interaction, key: string];
  silence: [body: SilenceBody];
}>();

const { t } = useI18n();
const reply = ref("");

/** Whether this user has already acknowledged under a key (§2.3, T4). */
const acked = computed(() => {
  const keys = new Set<string>();
  for (const answer of props.answers) {
    if (answer.interaction.type === "ack") {
      keys.add(answer.interaction.key);
    }
  }
  return keys;
});

/** The option picked for each `choice` key — last pick wins (§2.2). */
const picked = computed(() => {
  const chosen = new Map<string, string>();
  for (const answer of props.answers) {
    if (answer.interaction.type === "choice") {
      chosen.set(answer.interaction.key, answer.interaction.option);
    }
  }
  return chosen;
});

/** Durations offered by the silence menu — the ones people actually pick. */
const DURATIONS = computed<{ label: string; value: string }[]>(() => [
  { label: t("notification.actionBar.silenceDurations.1h"), value: "1h" },
  { label: t("notification.actionBar.silenceDurations.4h"), value: "4h" },
  { label: t("notification.actionBar.silenceDurations.24h"), value: "24h" },
  { label: t("notification.actionBar.silenceDurations.7d"), value: "7d" },
]);

function send(): void {
  const text = reply.value.trim();
  if (!text) {
    return;
  }
  emit("act", { type: "reply", text }, "reply");
  reply.value = "";
}
</script>

<template>
  <div class="flex flex-wrap items-center gap-2">
    <template v-for="(action, position) in actions" :key="position">
      <!-- view: client-side only. The server restricts the URL to http(s) (§2.3);
           it is checked again here, because an `href` is where `javascript:` runs. -->
      <Button
        v-if="action.type === 'view' && isWebUrl(action.url)"
        as="a"
        variant="outline"
        size="sm"
        :href="action.url"
        target="_blank"
        rel="noopener noreferrer"
      >
        <ExternalLink />
        {{ action.label }}
      </Button>

      <Button
        v-else-if="action.type === 'ack'"
        :variant="acked.has(action.key) ? 'secondary' : 'default'"
        size="sm"
        :disabled="acting !== null"
        @click="emit('act', { type: 'ack', key: action.key }, action.key)"
      >
        <Check v-if="acked.has(action.key)" />
        {{ acked.has(action.key) ? `${action.label} ✓` : action.label }}
      </Button>

      <div
        v-else-if="action.type === 'choice'"
        class="flex flex-wrap items-center gap-1"
        role="group"
        :aria-label="action.label"
      >
        <span class="mr-1 text-xs text-muted-foreground">{{ action.label }}</span>
        <Button
          v-for="option in action.options"
          :key="option.key"
          :variant="picked.get(action.key) === option.key ? 'default' : 'outline'"
          size="sm"
          :disabled="acting !== null"
          @click="
            emit(
              'act',
              { type: 'choice', key: action.key, option: option.key },
              `${action.key}:${option.key}`,
            )
          "
        >
          {{ option.label }}
        </Button>
      </div>

      <div v-else-if="action.type === 'reply'" class="flex w-full items-center gap-2">
        <Input
          v-model="reply"
          class="h-8 flex-1"
          :placeholder="action.placeholder ?? action.label"
          :disabled="acting !== null"
          @keydown.enter.prevent="send"
        />
        <Button
          variant="ghost"
          size="sm"
          :disabled="acting !== null || !reply.trim()"
          @click="send"
        >
          <Send />
          {{ action.label }}
        </Button>
      </div>

      <DropdownMenu v-else-if="action.type === 'silence'">
        <template #trigger>
          <Button variant="ghost" size="sm" :disabled="acting !== null">
            <BellOff />
            {{ action.label }}
          </Button>
        </template>
        <!-- A notification-scoped silence may run until the notification closes,
             which is why "jusqu'à la fermeture" only appears for that scope. -->
        <DropdownMenuItem
          v-if="action.scope === 'notification'"
          @select="emit('silence', { scope: 'notification' })"
        >
          {{ t("notification.actionBar.silenceUntilClose") }}
        </DropdownMenuItem>
        <DropdownMenuItem
          v-for="duration in DURATIONS"
          :key="duration.value"
          @select="emit('silence', { scope: action.scope, duration: duration.value })"
        >
          {{ duration.label }}
        </DropdownMenuItem>
      </DropdownMenu>
    </template>
  </div>
</template>
