<script setup lang="ts">
/**
 * Where a local popup appears (§7.4, §10.5).
 *
 * Deliberately small and hand-written rather than a primitive from reka-ui: this
 * has one behaviour — appear, linger, go — and the store already owns the timing
 * and the do-not-disturb gate. What is left is a list and a transition.
 *
 * Bottom-right, above everything, and never over the detail panel's actions on a
 * narrow screen: on mobile it takes the bottom of the viewport instead, where the
 * thumb is not.
 */
import { X } from "@lucide/vue";
import { useRouter } from "vue-router";

import { useToasts } from "@/stores/toasts";

const toasts = useToasts();
const router = useRouter();

function open(url: string | undefined, id: string): void {
  toasts.dismiss(id);
  if (url) {
    void router.push(url);
  }
}
</script>

<template>
  <!--
    `aria-live="polite"` and not `assertive`: a notification is worth reading, not
    worth interrupting what somebody is typing. Screen readers announce it when
    they reach a pause, which is the same courtesy the visual version extends.
  -->
  <div
    class="pointer-events-none fixed inset-x-3 bottom-3 z-50 flex flex-col gap-2 sm:inset-x-auto sm:right-4 sm:bottom-4 sm:w-90"
    role="region"
    aria-live="polite"
    aria-label="Notifications"
  >
    <TransitionGroup
      enter-active-class="transition duration-200 ease-out"
      enter-from-class="translate-y-2 opacity-0"
      leave-active-class="transition duration-150 ease-in"
      leave-to-class="translate-y-1 opacity-0"
    >
      <div
        v-for="toast in toasts.items"
        :key="toast.id"
        class="pointer-events-auto rounded-lg border border-border bg-popover p-3 text-popover-foreground shadow-lg"
      >
        <div class="flex items-start gap-2">
          <button
            type="button"
            class="min-w-0 flex-1 cursor-pointer text-left"
            @click="open(toast.url, toast.id)"
          >
            <p class="truncate text-sm font-medium">{{ toast.title }}</p>
            <p
              v-if="toast.body"
              class="mt-0.5 line-clamp-2 text-xs text-muted-foreground"
            >
              {{ toast.body }}
            </p>
          </button>
          <button
            type="button"
            class="shrink-0 rounded-sm p-1 text-muted-foreground hover:text-foreground"
            aria-label="Fermer"
            @click="toasts.dismiss(toast.id)"
          >
            <X class="size-3.5" />
          </button>
        </div>
      </div>
    </TransitionGroup>
  </div>
</template>
