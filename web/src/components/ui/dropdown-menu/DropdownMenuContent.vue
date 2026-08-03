<script setup lang="ts">
import { DropdownMenuContent } from "reka-ui";
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = withDefaults(
  defineProps<{
    align?: "start" | "center" | "end";
    class?: HTMLAttributes["class"];
  }>(),
  { align: "end" },
);

const classes = computed(() =>
  cn(
    "z-50 min-w-40 overflow-hidden rounded-md border border-border bg-popover p-1 text-popover-foreground shadow-md",
    "data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0",
    props.class,
  ),
);
</script>

<template>
  <DropdownMenuContent :align="align" :side-offset="4" :class="classes">
    <slot />
  </DropdownMenuContent>
</template>
