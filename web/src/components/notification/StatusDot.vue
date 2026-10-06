<script setup lang="ts">
/** The lifecycle as a pixel matrix: coarse 2×2 while running, a 3×3 status pattern once closed. Never colour alone (§10.3). */
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import type { State, Status } from "@/api/generated/types.gen";
import { lookOf, stateLabel } from "@/lib/notification";

const props = defineProps<{ status: Status; state: State; class?: string }>();

const { t } = useI18n();
const look = computed(() => lookOf(props.status));

// Lit cells of the closed 3×3, row-major.
const PATTERN: Record<Status, number[]> = {
  success: [0, 1, 2, 3, 4, 5, 6, 7, 8],
  failure: [0, 2, 4, 6, 8],
  warning: [1, 3, 4, 5, 7],
  info: [1, 4, 7],
  cancelled: [2, 4, 6],
};

const fine = computed(() => props.state === "closed");
const cells = computed(() => {
  if (fine.value) {
    const lit = PATTERN[props.status] ?? PATTERN.info;
    return Array.from({ length: 9 }, (_, i) => lit.includes(i));
  }
  return props.state === "working"
    ? [true, true, false, true]
    : [true, false, false, false];
});
</script>

<template>
  <span
    :class="['inline-grid shrink-0 gap-px', fine ? 'grid-cols-3' : 'grid-cols-2', look.color, props.class]"
    :aria-label="t('status.ariaLabel', { status: look.label, state: stateLabel(state) })"
    role="img"
  >
    <span
      v-for="(lit, i) in cells"
      :key="`${fine}-${i}`"
      :class="[fine ? 'size-[4px]' : 'size-[6.5px]', 'bg-current']"
      :style="{
        opacity: lit ? 1 : 0.18,
        animation: !fine && state === 'working' && lit ? `chase 1.6s ${i * 0.2}s steps(2, jump-none) infinite` : undefined,
      }"
    />
  </span>
</template>
