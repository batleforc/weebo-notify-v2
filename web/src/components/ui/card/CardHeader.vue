<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() => cn("flex flex-col gap-1.5 p-4", props.class));
</script>

<template>
  <div :class="classes"><slot /></div>
</template>
