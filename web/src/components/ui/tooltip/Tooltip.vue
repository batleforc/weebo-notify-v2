<script setup lang="ts">
/**
 * A tooltip, in one component instead of shadcn's four.
 *
 * The upstream split (Provider / Root / Trigger / Content) exists so a trigger can
 * be composed anywhere; every tooltip in this app is "hover this, read that", and
 * a single component with two slots is what the call sites actually want. Reka UI
 * still carries the behaviour — delay, focus, escape, ARIA (ADR-0004).
 */
import {
  TooltipContent,
  TooltipPortal,
  TooltipProvider,
  TooltipRoot,
  TooltipTrigger,
} from "reka-ui";

withDefaults(defineProps<{ side?: "top" | "right" | "bottom" | "left" }>(), {
  side: "top",
});
</script>

<template>
  <TooltipProvider :delay-duration="200">
    <TooltipRoot>
      <TooltipTrigger as-child>
        <slot />
      </TooltipTrigger>
      <TooltipPortal>
        <TooltipContent
          :side="side"
          :side-offset="6"
          class="z-50 max-w-xs border border-rule-strong bg-popover px-2 py-1 text-xs text-popover-foreground"
        >
          <slot name="content" />
        </TooltipContent>
      </TooltipPortal>
    </TooltipRoot>
  </TooltipProvider>
</template>
