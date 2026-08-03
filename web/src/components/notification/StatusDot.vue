<script setup lang="ts">
/**
 * Status as a coloured pip **and** an icon (§10.3) — never colour alone.
 *
 * The lifecycle shows here too: `open` and `working` pulse discreetly so a live
 * incident reads as live, and `closed` is still. No blinking, even for `urgent`.
 */
import { computed } from "vue";

import type { State, Status } from "@/api/generated/types.gen";
import { lookOf, stateLabel } from "@/lib/notification";

const props = defineProps<{ status: Status; state: State; class?: string }>();

const look = computed(() => lookOf(props.status));
const live = computed(() => props.state !== "closed");
</script>

<template>
  <span
    :class="['relative inline-flex items-center justify-center', look.color, props.class]"
    :aria-label="`${look.label}, ${stateLabel(state)}`"
    role="img"
  >
    <component :is="look.icon" class="size-4" />
    <span
      v-if="live"
      class="absolute -right-0.5 -top-0.5 size-1.5 animate-pulse rounded-full bg-current"
      aria-hidden="true"
    />
  </span>
</template>
