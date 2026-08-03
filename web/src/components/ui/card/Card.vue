<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() =>
  cn(
    "rounded-lg border border-border bg-card text-card-foreground",
    props.class,
  ),
);
</script>

<template>
  <div :class="classes"><slot /></div>
</template>
