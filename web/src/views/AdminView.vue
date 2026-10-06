<script setup lang="ts">
/**
 * Channels, devices, API keys and service accounts (§10.4, §5.2, §5.2bis).
 *
 * Four lists on one screen because they are one question — *what can act as me, or
 * for my team, and under what rules* — and four screens would hide that. §10.4bis's
 * platform console is a different thing entirely: it is instance-wide and is M4.
 *
 * # The secret is shown once, and the screen says so before it is minted
 *
 * §5.2: only the hash is stored, so a key that is not copied now is a key that has
 * to be revoked and recreated. Saying that *after* showing it would be an apology;
 * saying it on the button is a warning.
 *
 * # The screen does not decide who may edit a policy
 *
 * §5.1: authorisation is derived per request from the token, never held locally, so
 * this view offers the controls and renders the refusal if one comes back. What it
 * *does* hide is what the data itself settles: a managed channel gets no controls at
 * all, because config owns it (§11) and an API call would be undone at the next
 * reconciliation.
 */
import { KeyRound, Plus, Trash2 } from "@lucide/vue";
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import {
  createAccount,
  createKey,
  deactivateAccount,
  listAccounts,
  listDevices,
  listKeys,
  revokeDevice,
  revokeKey,
  setKeyLimit,
} from "@/api/generated";
import type { ApiKey, Device, ServiceAccount } from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import ChannelAdmin from "@/components/admin/ChannelAdmin.vue";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, exact } from "@/lib/format";
import { useChannels } from "@/stores/channels";
import { useConsole } from "@/stores/console";

const devices = ref<Device[]>([]);
const keys = ref<ApiKey[]>([]);
const accounts = ref<ServiceAccount[]>([]);
const error = ref<string | null>(null);
const busy = ref(false);

const channels = useChannels();
const platform = useConsole();
const { t } = useI18n();

/** The secret, for as long as this screen is open. Never stored, never refetched. */
const minted = ref<string | null>(null);

const newKeyLabel = ref("");
const newKeyAccount = ref("");
const newKeyChannel = ref("");
const newAccountId = ref("");

// A disabled account cannot authenticate a key minted for it (§5.2, T25): a
// picker that offered one anyway would let the form succeed and the first use
// fail, with nothing on this screen explaining why.
const activeAccounts = computed(() =>
  accounts.value.filter((account) => !account.disabled_at),
);

async function load(): Promise<void> {
  error.value = null;
  try {
    const [device, key, account] = await Promise.all([
      listDevices({ throwOnError: true }),
      listKeys({ throwOnError: true }),
      listAccounts({ throwOnError: true }),
      channels.load(),
    ]);
    devices.value = device.data;
    keys.value = key.data;
    accounts.value = account.data;
  } catch (caught) {
    error.value = failureOf(caught).message;
  }
}

async function act(work: () => Promise<unknown>): Promise<void> {
  busy.value = true;
  error.value = null;
  try {
    await work();
    await load();
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    busy.value = false;
  }
}

/**
 * Mint a key **owned by a service account**, never by the person clicking.
 *
 * That is §5.2's rule made into a UI: a team bot on a human's key dies the day
 * that human is offboarded. The screen therefore asks which bot the key acts as,
 * and a personal key stays a deliberate act somewhere else.
 */
async function mintKey(): Promise<void> {
  if (!newKeyAccount.value || !newKeyChannel.value.trim()) {
    return;
  }
  await act(async () => {
    const answer = await createKey({
      body: {
        label: newKeyLabel.value.trim() || null,
        owner: { owner: "service_account", id: newKeyAccount.value },
        channels: [newKeyChannel.value.trim()],
        scopes: ["publish"],
      },
      throwOnError: true,
    });
    // The one moment this value exists outside the caller's hands (§5.2).
    minted.value = answer.data.secret;
    newKeyLabel.value = "";
  });
}

async function addAccount(): Promise<void> {
  if (!newAccountId.value.trim()) {
    return;
  }
  await act(async () => {
    await createAccount({
      body: { id: newAccountId.value.trim() },
      throwOnError: true,
    });
    newAccountId.value = "";
  });
}

/** Per-key ceiling (§8.2, D24) — platform-admin only; empty clears it. */
const limits = ref<Record<string, string>>({});

function saveLimit(key: ApiKey): void {
  const raw = (limits.value[key.id] ?? "").trim();
  void act(() =>
    setKeyLimit({
      path: { id: key.id },
      body: { publish_per_minute: raw ? Number(raw) : null },
      throwOnError: true,
    }),
  );
}

onMounted(load);
</script>

<template>
  <div class="h-full overflow-y-auto">
    <div class="mx-auto max-w-4xl px-4 pb-16 pt-6 sm:px-8 sm:pt-10">
      <h1 class="display mb-6 uppercase">{{ t("admin.title") }}</h1>
      <p v-if="error" class="mb-4 border border-signal px-4 py-3 text-sm text-signal" role="alert">{{ error }}</p>

      <!-- Shown once, and only here: only the hash is stored (§5.2). -->
      <section v-if="minted" class="mb-6 space-y-3 border border-copper p-4">
        <p class="pixel text-base font-bold uppercase text-copper">{{ t("admin.mintedKey.title") }}</p>
        <p class="text-sm text-ink-dim">{{ t("admin.mintedKey.description") }}</p>
        <code class="block select-all border border-rule bg-ground-sunk p-3 text-xs [overflow-wrap:anywhere]">{{ minted }}</code>
        <Button variant="outline" size="sm" @click="minted = null">{{ t("admin.mintedKey.copied") }}</Button>
      </section>

      <Card>
        <CardHeader>
          <CardTitle>{{ t("admin.channels.title") }}</CardTitle>
          <p class="max-w-[64ch] text-sm text-ink-dim">{{ t("admin.channels.description") }}</p>
        </CardHeader>
        <CardContent>
          <ChannelAdmin :busy="busy" @act="act" />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{{ t("admin.accounts.title") }}</CardTitle>
          <p class="max-w-[64ch] text-sm text-ink-dim">{{ t("admin.accounts.description") }}</p>
        </CardHeader>
        <CardContent class="space-y-3">
          <form class="flex gap-2" @submit.prevent="addAccount">
            <Input v-model="newAccountId" placeholder="bot-argocd" class="flex-1" :aria-label="t('admin.accounts.title')" />
            <Button type="submit" :disabled="busy || !newAccountId.trim()">
              <Plus />
              {{ t("admin.accounts.create") }}
            </Button>
          </form>

          <ul class="border-t border-rule">
            <li v-for="account in accounts" :key="account.id" class="rule-row space-y-1 py-3 text-sm">
              <div class="flex flex-wrap items-center gap-2">
                <span class="min-w-0 flex-1">
                  <span>{{ account.display_name || account.id }}</span>
                  <code class="ml-2 text-xs text-ink-dim">{{ account.id }}</code>
                </span>
                <Badge v-if="account.origin === 'managed'" variant="outline">gitops</Badge>
                <Badge v-if="account.disabled_at" variant="muted">{{ t("admin.accounts.disabled") }}</Badge>
                <!-- Config is a managed account's source of truth (§11): no button. -->
                <Button
                  v-else-if="account.origin !== 'managed'"
                  variant="ghost"
                  size="sm"
                  :disabled="busy"
                  @click="act(() => deactivateAccount({ path: { id: account.id }, throwOnError: true }))"
                >
                  <Trash2 />
                  {{ t("admin.accounts.deactivate") }}
                </Button>
              </div>
              <div class="flex flex-wrap items-center gap-x-2 text-xs text-ink-dim">
                <span v-if="account.description">{{ account.description }} ·</span>
                <Tooltip>
                  <span>{{ t("admin.accounts.created", { at: ago(account.created_at) }) }}</span>
                  <template #content>{{ exact(account.created_at) }}</template>
                </Tooltip>
                <span v-if="account.created_by">· {{ t("admin.accounts.owner") }} <code>{{ account.created_by }}</code></span>
              </div>
            </li>
          </ul>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{{ t("admin.keys.title") }}</CardTitle>
        </CardHeader>
        <CardContent class="space-y-3">
          <!-- Only service accounts own these: a durable bot never runs on a human's key (§5.2). -->
          <form class="grid gap-2 sm:grid-cols-3" @submit.prevent="mintKey">
            <select
              v-model="newKeyAccount"
              class="h-9 border border-rule-strong bg-ground-sunk px-3 text-sm pointer-coarse:h-11"
              :aria-label="t('admin.keys.accountPlaceholder')"
            >
              <option value="">{{ t("admin.keys.accountPlaceholder") }}</option>
              <option v-for="account in activeAccounts" :key="account.id" :value="account.id">
                {{ account.display_name || account.id }}
              </option>
            </select>
            <select
              v-model="newKeyChannel"
              class="h-9 border border-rule-strong bg-ground-sunk px-3 text-sm pointer-coarse:h-11"
              :aria-label="t('admin.keys.channelPlaceholder')"
            >
              <option value="">{{ t("admin.keys.channelPlaceholder") }}</option>
              <option v-for="channel in channels.all" :key="channel.id" :value="channel.id">
                {{ channel.display_name || channel.id }}
              </option>
            </select>
            <Input v-model="newKeyLabel" :placeholder="t('admin.keys.labelPlaceholder')" />
            <Button type="submit" class="sm:col-span-3 sm:justify-self-start" :disabled="busy || !newKeyAccount || !newKeyChannel.trim()">
              <KeyRound />
              {{ t("admin.keys.create") }}
            </Button>
          </form>

          <ul class="border-t border-rule">
            <li v-for="key in keys" :key="key.id" class="rule-row flex flex-wrap items-center gap-2 py-3 text-sm">
              <span class="min-w-0 flex-1">
                <code class="text-xs">{{ key.prefix }}…</code>
                <span class="ml-2">{{ key.label ?? t("admin.keys.unnamed") }}</span>
                <span class="block text-xs text-ink-dim">
                  {{ key.channels.join(", ") }} ·
                  {{ key.last_used_at ? t("admin.keys.lastUsed", { at: exact(key.last_used_at) }) : t("admin.keys.neverUsed") }}
                </span>
              </span>
              <Badge v-if="key.revoked_at" variant="muted">{{ t("admin.keys.revoked") }}</Badge>
              <template v-else>
                <form v-if="platform.allowed" class="flex items-center gap-1" @submit.prevent="saveLimit(key)">
                  <Input
                    v-model="limits[key.id]"
                    inputmode="numeric"
                    class="h-8 w-24"
                    :placeholder="key.publish_per_minute ? String(key.publish_per_minute) : t('admin.keys.limitPlaceholder')"
                    :aria-label="t('admin.keys.limit')"
                  />
                  <Button type="submit" variant="outline" size="sm" :disabled="busy">{{ t("admin.keys.setLimit") }}</Button>
                </form>
                <Button
                  variant="ghost"
                  size="sm"
                  :disabled="busy"
                  @click="act(() => revokeKey({ path: { id: key.id }, throwOnError: true }))"
                >
                  <Trash2 />
                  {{ t("admin.devices.revoke") }}
                </Button>
              </template>
            </li>
          </ul>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{{ t("admin.devices.title") }}</CardTitle>
          <p class="max-w-[64ch] text-sm text-ink-dim">{{ t("admin.devices.description") }}</p>
        </CardHeader>
        <CardContent>
          <p v-if="!devices.length" class="text-sm text-ink-dim">{{ t("admin.devices.empty") }}</p>
          <ul class="border-t border-rule">
            <li v-for="device in devices" :key="device.id" class="rule-row flex items-center gap-2 py-3 text-sm">
              <span class="min-w-0 flex-1 truncate">{{ device.label }}</span>
              <span class="meta">{{ device.transport }}</span>
              <Badge v-if="device.revoked_at" variant="muted">{{ t("admin.devices.revoked") }}</Badge>
              <Button
                v-else
                variant="ghost"
                size="sm"
                :disabled="busy"
                @click="act(() => revokeDevice({ path: { id: device.id }, throwOnError: true }))"
              >
                <Trash2 />
                {{ t("admin.devices.revoke") }}
              </Button>
            </li>
          </ul>
        </CardContent>
      </Card>
    </div>
  </div>
</template>
