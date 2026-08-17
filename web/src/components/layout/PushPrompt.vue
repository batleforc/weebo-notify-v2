<script setup lang="ts">
/**
 * The boot-time ask: "want push on this browser?" (§6.2, M3.6).
 *
 * Shown at most once per browser profile — `PROMPTED_KEY` is set the moment this
 * decides to show, not on dismiss, so leaving without clicking anything still
 * counts as asked. Settings › Notifications (`PushPanel.vue`) is where the switch
 * lives for good; this is only the one nudge toward finding it.
 *
 * Gated on the same three things `PushPanel` reads from `push.state()` — supported,
 * available, permission still "default" — so this never asks for a permission the
 * browser would refuse to raise again, or a deployment cannot honour.
 */
import { BellRing, X } from "@lucide/vue";
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import { Button } from "@/components/ui/button";
import { enable, PushRefused, state } from "@/lib/push";

const PROMPTED_KEY = "weebo-notify:push-prompted";

const { t } = useI18n();
const visible = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);

onMounted(async () => {
  if (localStorage.getItem(PROMPTED_KEY)) {
    return;
  }

  const status = await state();
  if (status.supported && status.available && status.permission === "default") {
    localStorage.setItem(PROMPTED_KEY, "1");
    visible.value = true;
  }
});

function dismiss(): void {
  visible.value = false;
}

async function accept(): Promise<void> {
  busy.value = true;
  error.value = null;
  try {
    await enable();
    visible.value = false;
  } catch (failure) {
    error.value =
      failure instanceof PushRefused
        ? failure.message
        : t("push.prompt.fallbackError");
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div
    v-if="visible"
    class="pointer-events-none fixed inset-x-3 top-3 z-50 flex justify-center sm:inset-x-auto sm:top-auto sm:bottom-4 sm:left-4 sm:justify-start"
    role="region"
    :aria-label="t('push.prompt.ariaLabel')"
  >
    <div
      class="pointer-events-auto w-full max-w-sm rounded-lg border border-border bg-popover p-3 text-popover-foreground shadow-lg"
    >
      <div class="flex items-start gap-2">
        <BellRing class="mt-0.5 size-4 shrink-0 text-primary" />
        <div class="min-w-0 flex-1">
          <p class="text-sm font-medium">{{ t("push.prompt.title") }}</p>
          <p class="mt-0.5 text-xs text-muted-foreground">
            {{ t("push.prompt.body") }}
          </p>
          <p v-if="error" class="mt-1 text-xs text-destructive">{{ error }}</p>
          <div class="mt-2 flex items-center gap-2">
            <Button size="sm" :disabled="busy" @click="accept">
              {{ busy ? "…" : t("push.prompt.activate") }}
            </Button>
            <Button size="sm" variant="ghost" :disabled="busy" @click="dismiss">
              {{ t("push.prompt.later") }}
            </Button>
          </div>
        </div>
        <button
          type="button"
          class="shrink-0 rounded-sm p-1 text-muted-foreground hover:text-foreground"
          :aria-label="t('push.prompt.close')"
          @click="dismiss"
        >
          <X class="size-3.5" />
        </button>
      </div>
    </div>
  </div>
</template>
