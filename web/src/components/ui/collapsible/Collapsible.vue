<script setup lang="ts">
/**
 * A collapsible section, in one component instead of shadcn's three.
 *
 * Same reduction as `Tooltip`/`HoverCard` next door (ADR-0004): every call site is
 * "click this heading, show or hide that". Reka UI carries the behaviour — the
 * open state, the measured height the CSS animation drives via
 * `--reka-collapsible-content-height`, and ARIA (`aria-expanded`,
 * `aria-controls`) on the trigger for free.
 */
import {
  CollapsibleContent,
  CollapsibleRoot,
  CollapsibleTrigger,
} from "reka-ui";

const open = defineModel<boolean>("open", { default: false });
</script>

<template>
  <CollapsibleRoot v-model:open="open">
    <CollapsibleTrigger as-child>
      <slot />
    </CollapsibleTrigger>
    <CollapsibleContent
      class="overflow-hidden data-[state=closed]:animate-collapsible-up data-[state=open]:animate-collapsible-down"
    >
      <slot name="content" />
    </CollapsibleContent>
  </CollapsibleRoot>
</template>
