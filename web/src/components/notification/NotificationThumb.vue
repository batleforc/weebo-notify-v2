<script setup lang="ts">
/**
 * The small square a list row shows for a notification's image (§2.3, §10.3, D30).
 *
 * Two kinds, two paths, and only one of them looks like `NotificationImage.vue`
 * next door. A `data_uri` is already small (capped by `MAX_DATA_URI_BYTES`) and
 * goes straight into `src`, same as the detail panel. An `attachment` resolves
 * through `lib/thumbnail.ts` — the small *derived* square — and never through
 * `lib/attachment.ts`'s presigned download of the full object: pulling that per
 * visible row of a virtualised list is exactly the bandwidth incident D30
 * exists to avoid.
 *
 * Silent rather than captioned when there is nothing to show, unlike the detail
 * panel: a row that cannot show a thumbnail (nothing was ever derived for an
 * attachment, say) simply omits the square. There is no room on a single row
 * for an explanation, and the title still reads without it.
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
