<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() =>
  cn("border-t border-rule bg-transparent text-card-foreground", props.class),
);
</script>

<template>
  <div :class="classes"><slot /></div>
</template>
