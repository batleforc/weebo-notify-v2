<script setup lang="ts">
/** Desktop only (RFC 0001): which server this app uses, and how to leave it. */
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { useSession } from "@/auth/session";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { apiBaseUrl } from "@/config";
import { forgetInstance, isPresetInstance } from "@/lib/instance";

const { t } = useI18n();
const session = useSession();
const busy = ref(false);
const preset = isPresetInstance();

async function changeServer(): Promise<void> {
  busy.value = true;
  // Sign out (which revokes the token) first, then forget the instance; the
  // reload at the end of sign-out lands on the first-launch screen.
  await session.logout({ afterSignOut: forgetInstance });
}
</script>

<template>
  <Card>
    <CardHeader>
      <CardTitle>{{ t("settings.server.title") }}</CardTitle>
    </CardHeader>
    <CardContent class="space-y-4">
      <p class="text-sm">
        {{ t("settings.server.current") }}
        <span class="font-mono">{{ apiBaseUrl }}</span>
      </p>
      <p v-if="preset" class="text-sm text-ink-dim">
        {{ t("settings.server.preset") }}
      </p>
      <Button v-else variant="outline" :disabled="busy" @click="changeServer">
        {{ t("settings.server.change") }}
      </Button>
    </CardContent>
  </Card>
</template>
