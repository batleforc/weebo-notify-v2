<script setup lang="ts">
/**
 * A notification's image (§2.3, D26): a placeholder while resolving (no layout
 * jump, §10.5), then the image or a visible "unavailable" caption. A silent miss is
 * often the CSP: `img-src` needs `WNK_ATTACHMENT_ORIGIN` (`lib/csp.ts`).
 */
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import type { Image } from "@/api/generated/types.gen";
import { imageSrc } from "@/lib/attachment";

const props = defineProps<{ image: Image }>();

const { t } = useI18n();
const src = ref<string | null>(null);
const failed = ref(false);
const resolving = ref(false);

/** Identity across re-renders, so an attachment isn't re-resolved on every render. */
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
    class="h-32 w-full max-w-sm border border-rule bg-ground-raised"
    aria-hidden="true"
  />

  <img
    v-else-if="src && !failed"
    :src="src"
    alt=""
    class="max-h-72 border border-rule object-contain"
    @error="failed = true"
  />

  <p v-else class="text-xs text-ink-dim">
    {{ t("notification.image.failed") }}
  </p>
</template>
