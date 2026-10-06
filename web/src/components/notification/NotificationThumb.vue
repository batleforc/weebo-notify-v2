<script setup lang="ts">
/**
 * A row's image thumbnail (§2.3, D30). Attachments use the derived thumbnail
 * (`lib/thumbnail.ts`), never the full presigned download. Silent when unavailable.
 */
import { computed, ref, watch } from "vue";

import type { Image } from "@/api/generated/types.gen";
import { thumbnailObjectUrl } from "@/lib/thumbnail";

const props = defineProps<{ image: Image }>();

const src = ref<string | null>(null);

/** See `NotificationImage.vue`: what identifies "the same image" across re-renders. */
const key = computed(() =>
  props.image.kind === "data_uri" ? props.image.uri : props.image.ref,
);

watch(
  key,
  async () => {
    if (props.image.kind === "data_uri") {
      src.value = props.image.uri;
      return;
    }
    src.value = await thumbnailObjectUrl(props.image.ref);
  },
  { immediate: true },
);
</script>

<template>
  <img
    v-if="src"
    :src="src"
    alt=""
    class="size-10 shrink-0 border border-rule object-cover"
  />
</template>
