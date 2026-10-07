<script setup lang="ts">
/** Local popups (§7.4, §10.5); timing and the DND gate live in the toasts store. */
import { X } from "@lucide/vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import { useToasts } from "@/stores/toasts";

const toasts = useToasts();
const router = useRouter();
const { t } = useI18n();

function open(url: string | undefined, id: string): void {
  toasts.dismiss(id);
  if (url) {
    void router.push(url);
  }
}
</script>

<template>
  <!-- `polite`: worth reading, not worth interrupting typing. -->
  <div
    class="pointer-events-none fixed inset-x-3 bottom-[calc(4.5rem+env(safe-area-inset-bottom))] z-50 flex flex-col gap-2 sm:inset-x-auto sm:right-4 sm:w-90 lg:bottom-4"
    role="region"
    aria-live="polite"
    :aria-label="t('toaster.region')"
  >
    <TransitionGroup
      leave-active-class="transition-opacity duration-150"
      leave-to-class="opacity-0"
    >
      <div
        v-for="toast in toasts.items"
        :key="toast.id"
        class="resolve pointer-events-auto border border-rule-strong bg-popover p-3 text-popover-foreground"
        @mouseenter="toasts.hold(toast.id)"
        @mouseleave="toasts.release(toast.id)"
        @focusin="toasts.hold(toast.id)"
        @focusout="toasts.release(toast.id)"
      >
        <div class="flex items-start gap-2">
          <button
            type="button"
            class="min-w-0 flex-1 cursor-pointer text-left"
            @click="open(toast.url, toast.id)"
          >
            <p class="truncate text-sm font-bold">{{ toast.title }}</p>
            <p
              v-if="toast.body"
              class="mt-0.5 line-clamp-2 text-xs text-ink-dim"
            >
              {{ toast.body }}
            </p>
          </button>
          <button
            type="button"
            class="shrink-0 p-1 text-ink-dim hover:text-ink"
            :aria-label="t('toaster.close')"
            @click="toasts.dismiss(toast.id)"
          >
            <X class="size-3.5" />
          </button>
        </div>
      </div>
    </TransitionGroup>
  </div>
</template>
