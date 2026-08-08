<script setup lang="ts">
/**
 * A notification body.
 *
 * `v-html` on sanitised output: the sanitising is in `lib/markdown.ts` and is the
 * only place allowed to produce HTML for this component. Passing anything else
 * here is the mistake to look for in review.
 */
import { computed } from "vue";

import { renderMarkdown } from "@/lib/markdown";

const props = defineProps<{ source: string; class?: string }>();

const html = computed(() => renderMarkdown(props.source));
</script>

<template>
  <!--
    `data-selectable` is what exempts this from the desktop build's
    `user-select: none` (§10bis.3bis): the app's chrome is not draggable-select,
    but a notification body is *content* — somebody wants to copy the error out of
    it. Marked here rather than in the stylesheet because this component is the one
    place that knows the difference.
  -->
  <!-- eslint-disable-next-line vue/no-v-html -->
  <div
    data-selectable
    :class="[
      'text-sm leading-relaxed [&_a]:text-primary [&_a]:underline [&_a]:underline-offset-2',
      '[&_code]:rounded-md [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-xs',
      '[&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:bg-muted [&_pre]:p-3',
      '[&_ul]:list-disc [&_ul]:pl-5 [&_ol]:list-decimal [&_ol]:pl-5',
      '[&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground',
      '[&_p]:my-1.5 [&_a[data-blocked]]:pointer-events-none [&_a[data-blocked]]:text-muted-foreground',
      props.class,
    ]"
    v-html="html"
  />
</template>
