<script setup lang="ts">
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{
  class?: HTMLAttributes["class"];
  modelValue?: string;
}>();
const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const classes = computed(() =>
  cn(
    "flex h-9 w-full min-w-0 border border-rule-strong bg-ground-sunk px-3 py-1 text-sm text-ink transition-colors placeholder:text-ink-dim focus-visible:border-signal disabled:cursor-not-allowed disabled:opacity-50 pointer-coarse:h-11 pointer-coarse:text-base",
    props.class,
  ),
);
</script>

<template>
  <input
    :class="classes"
    :value="modelValue"
    @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
  />
</template>
