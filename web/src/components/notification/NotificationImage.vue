<script setup lang="ts">
/**
 * The image a notification carries (§2.3, §10.3) — D26.
 *
 * # Three states, and the third is the one worth writing
 *
 *   * resolving — a box of the right shape rather than nothing, so the row does
 *     not jump when the bytes arrive. §10.5 asks for motion that never moves
 *     content the reader is already looking at, and an image appearing into a
 *     zero-height gap is exactly that;
 *   * shown;
 *   * **unavailable** — and it says so. A publisher who attached a screenshot and
 *     sees a caption saying the image could not be loaded knows to look at the
 *     bucket; one who sees nothing at all concludes their `--image` was ignored,
 *     which is the reading this component exists to end.
 *
 * The third state has two causes worth telling apart in a browser console and not
 * on screen: the signature could not be obtained (server, bucket, expiry), or the
 * bytes were refused by the page's own Content-Security-Policy. The second is
 * silent in a way the first is not — `img-src` has to name the origin the
 * presigns point at (`VITE_ATTACHMENT_ORIGIN`, `lib/csp.ts`), and a deployment
 * that forgets it gets a page that loads, works, and shows no attachment.
 */
import { computed, ref, watch } from "vue";

import type { Image } from "@/api/generated/types.gen";
import { imageSrc } from "@/lib/attachment";

const props = defineProps<{ image: Image }>();

const src = ref<string | null>(null);
const failed = ref(false);
const resolving = ref(false);

/**
 * What identifies "the same image" across re-renders.
 *
 * A `data:` URI is its own identity; an attachment is its reference. Watching the
 * object itself would re-resolve on every parent render, which for an attachment
 * means a request per render.
 */
const key = computed(() =>
  props.image.kind === "data_uri" ? props.image.uri : props.image.ref,
);

watch(
  key,
  async () => {
    resolving.value = true;
    failed.value = false;
    const resolved = await imageSrc(props.image);
    src.value = resolved;
    failed.value = resolved === null;
    resolving.value = false;
  },
  { immediate: true },
);
</script>

<template>
  <div
    v-if="resolving"
    class="h-32 w-full max-w-sm animate-pulse rounded-md border border-border bg-muted"
    aria-hidden="true"
  />

  <img
    v-else-if="src && !failed"
    :src="src"
    alt=""
    class="max-h-64 rounded-md border border-border object-contain"
    @error="failed = true"
  />

  <p v-else class="text-xs italic text-muted-foreground">
    L'image jointe n'a pas pu être affichée.
  </p>
</template>
