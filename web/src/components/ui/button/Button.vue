<script setup lang="ts">
import { Primitive, type PrimitiveProps } from "reka-ui";
import { computed, type HTMLAttributes } from "vue";

import { cn } from "@/lib/utils";
import { type ButtonVariants, buttonVariants } from ".";

const props = withDefaults(
  defineProps<
    PrimitiveProps & {
      variant?: ButtonVariants["variant"];
      size?: ButtonVariants["size"];
      class?: HTMLAttributes["class"];
    }
  >(),
  { as: "button" },
);

const classes = computed(() =>
  cn(buttonVariants({ variant: props.variant, size: props.size }), props.class),
);
</script>

<template>
  <Primitive :as="as" :as-child="asChild" :class="classes">
    <slot />
  </Primitive>
</template>
