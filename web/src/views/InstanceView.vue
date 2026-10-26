<script setup lang="ts">
/**
 * First launch of the desktop app (RFC 0001): which weebo-notify server to use.
 * The shell checks the address and the identity provider it names, then asks
 * the person in a native dialog; this screen only collects the address.
 */
import { ref } from "vue";
import { useI18n } from "vue-i18n";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { confirmInstance, probeInstance } from "@/lib/instance";

const { t } = useI18n();
const address = ref("https://");
const busy = ref(false);
const error = ref<string | null>(null);

async function choose(): Promise<void> {
  busy.value = true;
  error.value = null;
  try {
    const probed = await probeInstance(address.value);
    if (await confirmInstance(probed.probe)) {
      // A reload, so the shell serves the app with this instance's CSP.
      window.location.reload();
      return;
    }
    error.value = t("instance.declined");
  } catch (caught) {
    error.value = t("instance.refused", { reason: String(caught) });
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <main class="flex min-h-screen items-center justify-center p-6">
    <Card class="w-full max-w-md">
      <CardHeader>
        <!-- The page's one heading, styled as a card title (CardTitle is an h3). -->
        <h1 class="pixel text-base font-bold uppercase leading-none">
          {{ t("instance.title") }}
        </h1>
      </CardHeader>
      <CardContent>
        <form class="space-y-4" :aria-busy="busy" @submit.prevent="choose">
          <p id="instance-help" class="text-sm text-muted-foreground">
            {{ t("instance.help") }}
          </p>
          <label class="block space-y-1 text-sm" for="instance-address">
            <span>{{ t("instance.address") }}</span>
          </label>
          <Input
            id="instance-address"
            v-model="address"
            type="url"
            inputmode="url"
            autocomplete="url"
            required
            :aria-invalid="error !== null || undefined"
            :aria-describedby="error ? 'instance-help instance-error' : 'instance-help'"
          />
          <p
            v-if="error"
            id="instance-error"
            role="alert"
            class="text-sm text-destructive"
          >
            {{ error }}
          </p>
          <Button type="submit" :disabled="busy">
            {{ busy ? t("instance.checking") : t("instance.continue") }}
          </Button>
        </form>
      </CardContent>
    </Card>
  </main>
</template>
