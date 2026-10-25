<script setup lang="ts">
/** The caller's live connections, across every replica, and the way to cut one (§8.2). */
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import { disconnectStream, listStreams } from "@/api/generated";
import type { Connection } from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { clientLabel } from "@/lib/client-label";
import { ago, exact } from "@/lib/format";

const { t } = useI18n();
const connections = ref<Connection[]>([]);
const error = ref<string | null>(null);
const busy = ref<string | null>(null);

async function load(): Promise<void> {
  error.value = null;
  try {
    // This device first, then the newest: the one you are after is rarely the oldest.
    connections.value = (await listStreams({ throwOnError: true })).data.sort(
      (a, b) =>
        Number(b.current) - Number(a.current) ||
        b.opened_at.localeCompare(a.opened_at),
    );
  } catch (caught) {
    error.value = failureOf(caught).message;
  }
}

async function disconnect(id: string): Promise<void> {
  busy.value = id;
  try {
    await disconnectStream({ path: { id }, throwOnError: true });
    await load();
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    busy.value = null;
  }
}

function credentialLabel(credential: string): string {
  const kind = credential.split(":")[0] ?? "";
  return ["session", "key", "ci", "device"].includes(kind)
    ? t(`settings.connections.credential.${kind}`)
    : credential;
}

onMounted(load);
</script>

<template>
  <Card>
    <CardHeader>
      <CardTitle>{{ t("settings.connections.title") }}</CardTitle>
      <p class="max-w-[64ch] text-sm text-ink-dim">{{ t("settings.connections.description") }}</p>
    </CardHeader>
    <CardContent class="space-y-3">
      <p v-if="error" class="text-sm text-signal" role="alert">{{ error }}</p>
      <p v-else-if="!connections.length" class="text-sm text-ink-dim">{{ t("settings.connections.empty") }}</p>

      <ul class="border-t border-rule">
        <li
          v-for="connection in connections"
          :key="connection.id"
          class="rule-row flex flex-wrap items-center gap-x-3 gap-y-1 py-3 text-sm"
        >
          <span class="min-w-0 flex-1">
            <span class="flex items-center gap-2">
              <span class="truncate">{{ clientLabel(connection.client) }}</span>
              <Badge v-if="connection.current" variant="secondary">{{ t("settings.connections.current") }}</Badge>
            </span>
            <span class="block text-xs text-ink-dim">
              {{ credentialLabel(connection.credential) }} · {{ connection.transport }} ·
              <span :title="exact(connection.opened_at)">{{ t("settings.connections.since", { at: ago(connection.opened_at) }) }}</span>
            </span>
          </span>
          <Button
            v-if="!connection.current"
            variant="destructive"
            size="sm"
            :disabled="busy !== null"
            @click="disconnect(connection.id)"
          >
            {{ t("settings.connections.disconnect") }}
          </Button>
        </li>
      </ul>

      <Button variant="outline" size="sm" @click="load">{{ t("settings.connections.refresh") }}</Button>
    </CardContent>
  </Card>
</template>
