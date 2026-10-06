<script setup lang="ts">
/** The mark (a 3×3 dot grid, the closed-success matrix) and the wordmark. */
defineProps<{ large?: boolean }>();
</script>

<template>
  <span class="inline-flex items-center gap-2 text-ink">
    <svg viewBox="0 0 16 16" :class="large ? 'size-6' : 'size-4'" aria-hidden="true">
      <g fill="currentColor">
        <rect v-for="i in 9" :key="i" :x="((i - 1) % 3) * 6" :y="Math.floor((i - 1) / 3) * 6" width="4" height="4" />
      </g>
      <rect x="6" y="6" width="4" height="4" fill="var(--signal)" />
    </svg>
    <span :class="['pixel font-bold', large ? 'text-2xl' : 'text-base']">weebo<span class="text-signal">.</span>notify</span>
  </span>
</template>
