<script setup lang="ts">
/** A hover card in one component instead of shadcn's four; stays open while the pointer moves into it (ADR-0004). */
import {
  HoverCardContent,
  HoverCardPortal,
  HoverCardRoot,
  HoverCardTrigger,
} from "reka-ui";

withDefaults(
  defineProps<{
    side?: "top" | "right" | "bottom" | "left";
    align?: "start" | "center" | "end";
  }>(),
  { side: "bottom", align: "start" },
);
</script>

<template>
  <HoverCardRoot :open-delay="150" :close-delay="120">
    <HoverCardTrigger as-child>
      <slot />
    </HoverCardTrigger>
    <HoverCardPortal>
      <HoverCardContent
        :side="side"
        :align="align"
        :side-offset="6"
        class="z-50 w-72 border border-rule-strong bg-popover p-4 text-popover-foreground"
      >
        <slot name="content" />
      </HoverCardContent>
    </HoverCardPortal>
  </HoverCardRoot>
</template>
