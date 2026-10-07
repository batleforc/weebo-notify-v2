<script setup lang="ts">
/** A collapsible section in one component instead of shadcn's three; Reka UI does the behaviour (ADR-0004). */
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
