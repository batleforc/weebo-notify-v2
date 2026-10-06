<script setup lang="ts">
/**
 * Push on this browser (§6.2, §10.4, M3.6).
 *
 * One switch, and the honesty around it: four different things stop a push from
 * arriving — this browser cannot, this deployment does not, permission was
 * refused, or nobody asked yet — and a switch that reads "off" for all four sends
 * people to look in the wrong place. Each says which one it is.
 *
 * Enrolment is a *device* (§15), not a preference: turning this on adds a row to
 * Devices in Admin, and revoking it there kills the subscription and the sync
 * cursor together. That is why the copy names it.
 */
import { BellOff, BellRing } from "@lucide/vue";
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  disable,
  enable,
  PushRefused,
  type PushState,
  state,
} from "@/lib/push";

const { t } = useI18n();
const status = ref<PushState | null>(null);
const busy = ref(false);
const error = ref<string | null>(null);

async function refresh(): Promise<void> {
  status.value = await state();
}

onMounted(refresh);

async function toggle(): Promise<void> {
  busy.value = true;
  error.value = null;
  try {
    if (status.value?.enabled) {
      await disable();
    } else {
      await enable();
    }
  } catch (failure) {
    error.value =
      failure instanceof PushRefused
        ? failure.message
        : t("settings.push.fallbackError");
  } finally {
    busy.value = false;
    await refresh();
  }
}
</script>

<template>
  <Card>
    <CardHeader>
      <CardTitle>{{ t("settings.push.title") }}</CardTitle>
    </CardHeader>
    <CardContent class="space-y-3 text-sm">
      <!-- The markup is a slot, so the locale file carries text and only text.
           This used to be `v-html` over an interpolated translation: correct as
           written, and correct only for as long as every interpolation stayed a
           literal. A slot makes that structural — and it fixed a real bug on the
           way, since the emphasised word was hardcoded English in every locale. -->
      <i18n-t
        keypath="settings.push.description"
        tag="p"
        scope="global"
        class="text-ink-dim"
      >
        <template #device>
          <strong>{{ t("settings.push.descriptionDevice") }}</strong>
        </template>
      </i18n-t>

      <p v-if="!status" class="text-ink-dim">
        {{ t("settings.push.checking") }}
      </p>

      <p v-else-if="!status.supported" class="text-ink-dim">
        {{ t("settings.push.unsupported") }}
      </p>

      <p v-else-if="!status.available" class="text-ink-dim">
        {{ t("settings.push.unavailable") }}
      </p>

      <p
        v-else-if="status.permission === 'denied'"
        class="text-ink-dim"
      >
        {{ t("settings.push.denied") }}
      </p>

      <div v-else class="flex items-center gap-2">
        <Button size="sm" :disabled="busy" @click="toggle">
          <component :is="status.enabled ? BellOff : BellRing" class="size-4" />
          {{ status.enabled ? t("settings.push.disable") : t("settings.push.enable") }}
        </Button>
        <span v-if="busy" class="text-xs text-ink-dim">…</span>
      </div>

      <p v-if="error" class="text-xs text-signal">{{ error }}</p>
    </CardContent>
  </Card>
</template>
