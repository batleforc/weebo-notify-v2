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
      'max-w-[68ch] text-sm leading-[1.7] [overflow-wrap:anywhere] [&_a]:text-signal [&_a]:underline',
      '[&_code]:bg-ground-sunk [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-xs',
      '[&_pre]:overflow-x-auto [&_pre]:border [&_pre]:border-rule [&_pre]:bg-ground-sunk [&_pre]:p-3 [&_pre_code]:p-0',
      '[&_ul]:list-[square] [&_ul]:pl-5 [&_ol]:list-decimal [&_ol]:pl-5',
      '[&_blockquote]:border-l [&_blockquote]:border-rule-strong [&_blockquote]:pl-3 [&_blockquote]:text-ink-dim',
      '[&_h1]:text-base [&_h1]:font-bold [&_h2]:font-bold [&_h3]:font-bold',
      '[&_p]:my-2 [&_a[data-blocked]]:pointer-events-none [&_a[data-blocked]]:text-ink-dim',
      props.class,
    ]"
    v-html="html"
  />
</template>
