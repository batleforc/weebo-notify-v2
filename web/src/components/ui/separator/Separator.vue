<script setup lang="ts">
import { Separator, type SeparatorProps } from "reka-ui";
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";

const props = withDefaults(
  defineProps<SeparatorProps & { class?: HTMLAttributes["class"] }>(),
  { orientation: "horizontal", decorative: true },
);

const classes = computed(() =>
  cn(
    "shrink-0 bg-border",
    props.orientation === "vertical" ? "h-full w-px" : "h-px w-full",
    props.class,
  ),
);
</script>

<template>
  <Separator
    :orientation="orientation"
    :decorative="decorative"
    :class="classes"
  />
</template>
