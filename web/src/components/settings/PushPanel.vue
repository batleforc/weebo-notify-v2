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
import { BellOff, BellRing } from "lucide-vue-next";
import { onMounted, ref } from "vue";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  disable,
  enable,
  PushRefused,
  type PushState,
  state,
} from "@/lib/push";

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
        : "L'abonnement a échoué. Réessayez.";
  } finally {
    busy.value = false;
    await refresh();
  }
}
</script>

<template>
  <Card>
    <CardHeader>
      <CardTitle>Notifications push</CardTitle>
    </CardHeader>
    <CardContent class="space-y-3 text-sm">
      <p class="text-muted-foreground">
        Recevoir les notifications même quand l'onglet est fermé. Ce navigateur
        devient un <strong>device</strong> : il apparaît dans Admin → Devices, et
        le révoquer coupe ses push et son curseur de sync.
      </p>

      <p v-if="!status" class="text-muted-foreground">Vérification…</p>

      <p v-else-if="!status.supported" class="text-muted-foreground">
        Ce navigateur ne sait pas recevoir de push — une fenêtre privée ou une
        origine non sécurisée, le plus souvent.
      </p>

      <p v-else-if="!status.available" class="text-muted-foreground">
        Ce déploiement ne fait pas de Web Push : aucune clé VAPID n'y est
        configurée. Le flux temps réel continue de fonctionner.
      </p>

      <p
        v-else-if="status.permission === 'denied'"
        class="text-muted-foreground"
      >
        Les notifications sont bloquées pour ce site. Le navigateur ne
        redemandera pas : il faut les réautoriser dans ses réglages de site.
      </p>

      <div v-else class="flex items-center gap-2">
        <Button size="sm" :disabled="busy" @click="toggle">
          <component :is="status.enabled ? BellOff : BellRing" class="size-4" />
          {{ status.enabled ? "Désactiver sur ce navigateur" : "Activer sur ce navigateur" }}
        </Button>
        <span v-if="busy" class="text-xs text-muted-foreground">…</span>
      </div>

      <p v-if="error" class="text-xs text-destructive">{{ error }}</p>
    </CardContent>
  </Card>
</template>
