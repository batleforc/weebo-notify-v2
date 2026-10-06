<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() =>
  cn("flex flex-wrap items-center gap-2 px-0 pb-5 pt-0", props.class),
);
</script>

<template>
  <div :class="classes"><slot /></div>
</template>
