<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{ class?: HTMLAttributes["class"] }>();
const classes = computed(() =>
  cn("pixel text-base font-bold uppercase leading-none", props.class),
);
</script>

<template>
  <h3 :class="classes"><slot /></h3>
</template>
