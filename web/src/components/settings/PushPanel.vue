<script setup lang="ts">
/**
 * Push on this browser (§6.2, §10.4): says which of unsupported / unavailable /
 * denied / not asked is in play. Enabling enrols a device (§15).
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
      <!-- A slot keeps markup out of the locale files. -->
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
