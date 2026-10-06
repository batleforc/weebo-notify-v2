<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() =>
  cn("flex flex-col gap-1 px-0 pb-3 pt-5", props.class),
);
</script>

<template>
  <div :class="classes"><slot /></div>
</template>
