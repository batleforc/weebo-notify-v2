<script setup lang="ts">
/**
 * A hover card, in one component instead of shadcn's four.
 *
 * Same reduction as `Tooltip` next door, and for the same reason: every call site
 * is "hover this, read that". What makes it a hover card rather than a tooltip is
 * what goes inside — a tooltip holds a sentence, this holds a list with its own
 * layout, and it stays open while the pointer travels into it so the list can be
 * read. Reka UI carries the behaviour: open and close delays, pointer grace area,
 * focus and escape (ADR-0004).
 */
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
        class="z-50 w-72 rounded-md border border-border bg-popover p-3 text-popover-foreground shadow-md"
      >
        <slot name="content" />
      </HoverCardContent>
    </HoverCardPortal>
  </HoverCardRoot>
</template>
