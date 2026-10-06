<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() => cn("bg-ground-raised", props.class));
</script>

<template>
  <div :class="classes" aria-hidden="true" />
</template>
