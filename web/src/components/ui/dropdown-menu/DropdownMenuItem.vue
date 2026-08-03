<script setup lang="ts">
import { DropdownMenuItem } from "reka-ui";
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = defineProps<{
  class?: HTMLAttributes["class"];
  disabled?: boolean;
}>();
const emit = defineEmits<{ select: [] }>();

const classes = computed(() =>
  cn(
    "relative flex cursor-default select-none items-center gap-2 rounded-sm px-2 py-1.5 text-sm outline-none data-[highlighted]:bg-accent data-[highlighted]:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50 [&_svg]:size-4",
    props.class,
  ),
);
</script>

<template>
  <DropdownMenuItem :class="classes" :disabled="disabled" @select="emit('select')">
    <slot />
  </DropdownMenuItem>
</template>
