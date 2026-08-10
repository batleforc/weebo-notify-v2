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
import { KeyRound, Plus, Trash2 } from "lucide-vue-next";
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
  updateChannel,
} from "@/api/generated";
import type {
  ApiKey,
  Channel,
  Device,
  ServiceAccount,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Tooltip } from "@/components/ui/tooltip";
import { ago, exact } from "@/lib/format";
import { useChannels } from "@/stores/channels";

const devices = ref<Device[]>([]);
const keys = ref<ApiKey[]>([]);
const accounts = ref<ServiceAccount[]>([]);
const error = ref<string | null>(null);
const busy = ref(false);

const channels = useChannels();
const { t } = useI18n();

/**
 * The TTL fields, per channel, while they are being edited.
 *
 * Keyed by channel id and seeded from the server's answer on every load: an input
 * that kept a stale value would show one policy and send another.
 */
const draft = ref<Record<string, { default_ttl: string; max_ttl: string }>>({});

function seedDrafts(): void {
  draft.value = Object.fromEntries(
    channels.all.map((channel) => [
      channel.id,
      {
        default_ttl: channel.policy.ttl.default,
        // An absent ceiling is an empty field, not the string "0": `0` is what you
        // *type* to lift one, and pre-filling it would make "leave it alone" and
        // "remove it" look identical.
        max_ttl: channel.policy.ttl.max ?? "",
      },
    ]),
  );
}

/**
 * Send only the knobs that moved (§8, `PATCH /channels/{id}`).
 *
 * Restating the untouched ones is how two admins on two screens overwrite each
 * other, and it is exactly what a PATCH exists to avoid.
 */
async function savePolicy(channel: Channel): Promise<void> {
  const edited = draft.value[channel.id];
  if (!edited) {
    return;
  }

  const body: Record<string, string> = {};
  if (edited.default_ttl && edited.default_ttl !== channel.policy.ttl.default) {
    body.default_ttl = edited.default_ttl;
  }
  if (edited.max_ttl !== (channel.policy.ttl.max ?? "")) {
    // An emptied field means "lift the ceiling", which the API spells `0` — the
    // same spelling §7.2bis gives unbounded retention everywhere else.
    body.max_ttl = edited.max_ttl || "0";
  }
  if (Object.keys(body).length === 0) {
    return;
  }

  await act(() =>
    updateChannel({ path: { channel: channel.id }, body, throwOnError: true }),
  );
}

async function toggleReceipts(channel: Channel): Promise<void> {
  await act(() =>
    updateChannel({
      path: { channel: channel.id },
      body: { receipts_enabled: !channel.policy.receipts_enabled },
      throwOnError: true,
    }),
  );
}

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
    seedDrafts();
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

onMounted(load);
</script>

<template>
  <div class="h-full space-y-4 overflow-y-auto p-6">
    <h1 class="text-lg font-semibold">{{ t("admin.title") }}</h1>
    <p v-if="error" class="text-sm text-destructive">{{ error }}</p>

    <!-- Shown once, and only here. Closing this card loses it for good, which is
         what "only the hash is stored" means in practice (§5.2). -->
    <Card v-if="minted" class="border-status-warning">
      <CardHeader>
        <CardTitle>{{ t("admin.mintedKey.title") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("admin.mintedKey.description") }}
        </p>
      </CardHeader>
      <CardContent class="space-y-2">
        <code class="block break-all rounded-md bg-muted p-3 text-xs">{{ minted }}</code>
        <Button variant="outline" size="sm" @click="minted = null">
          {{ t("admin.mintedKey.copied") }}
        </Button>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("admin.channels.title") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("admin.channels.description") }}
        </p>
      </CardHeader>
      <CardContent class="space-y-3">
        <p v-if="!channels.all.length" class="text-sm text-muted-foreground">
          {{ t("admin.channels.empty") }}
        </p>
        <div
          v-for="channel in channels.all"
          :key="channel.id"
          class="space-y-2 border-b border-border pb-3 text-sm last:border-0"
        >
          <div class="flex items-center justify-between gap-2">
            <span class="min-w-0">
              <span class="font-medium">{{ channel.display_name || channel.id }}</span>
              <code class="ml-2 text-xs text-muted-foreground">{{ channel.id }}</code>
              <Badge v-if="channel.origin === 'managed'" variant="muted" class="ml-2">
                managed
              </Badge>
            </span>
            <Button
              v-if="channel.origin !== 'managed'"
              variant="ghost"
              size="sm"
              :disabled="busy"
              @click="toggleReceipts(channel)"
            >
              {{ channel.policy.receipts_enabled ? t("admin.channels.receiptsOn") : t("admin.channels.receiptsOff") }}
            </Button>
            <span v-else class="text-xs text-muted-foreground">
              {{ t("admin.managedByConfig") }}
            </span>
          </div>

          <div
            v-if="channel.origin !== 'managed' && draft[channel.id]"
            class="grid gap-2 sm:grid-cols-3"
          >
            <Input
              v-model="draft[channel.id].default_ttl"
              :placeholder="t('admin.channels.defaultTtlPlaceholder')"
            />
            <!-- Vide = pas de plafond. On envoie `0`, qui est la façon dont
                 §7.2bis écrit « illimité » partout ailleurs. -->
            <Input
              v-model="draft[channel.id].max_ttl"
              :placeholder="t('admin.channels.maxTtlPlaceholder')"
            />
            <Button :disabled="busy" @click="savePolicy(channel)">
              {{ t("settings.profile.save") }}
            </Button>
          </div>
        </div>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("admin.devices.title") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("admin.devices.description") }}
        </p>
      </CardHeader>
      <CardContent class="space-y-2">
        <p v-if="!devices.length" class="text-sm text-muted-foreground">
          {{ t("admin.devices.empty") }}
        </p>
        <div
          v-for="device in devices"
          :key="device.id"
          class="flex items-center justify-between gap-2 border-b border-border pb-2 text-sm last:border-0"
        >
          <span class="min-w-0">
            <span class="font-medium">{{ device.label }}</span>
            <span class="ml-2 text-xs text-muted-foreground">{{ device.transport }}</span>
          </span>
          <Badge v-if="device.revoked_at" variant="muted">
            {{ t("admin.devices.revoked") }}
          </Badge>
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
        </div>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("admin.keys.title") }}</CardTitle>
      </CardHeader>
      <CardContent class="space-y-3">
        <!-- Owner first, and only service accounts: §5.2 says a durable bot never
             runs on a human's key, so the form does not offer to make one. -->
        <div class="grid gap-2 sm:grid-cols-3">
          <select
            v-model="newKeyAccount"
            class="h-9 rounded-md border border-input bg-background px-3 text-sm"
          >
            <option value="">{{ t("admin.keys.accountPlaceholder") }}</option>
            <option v-for="account in activeAccounts" :key="account.id" :value="account.id">
              {{ account.display_name || account.id }}
            </option>
          </select>
          <Input v-model="newKeyChannel" :placeholder="t('admin.keys.channelPlaceholder')" />
          <Input v-model="newKeyLabel" :placeholder="t('admin.keys.labelPlaceholder')" />
        </div>
        <Button
          :disabled="busy || !newKeyAccount || !newKeyChannel.trim()"
          @click="mintKey"
        >
          <KeyRound />
          {{ t("admin.keys.create") }}
        </Button>

        <div
          v-for="key in keys"
          :key="key.id"
          class="flex items-center justify-between gap-2 border-b border-border pb-2 text-sm last:border-0"
        >
          <span class="min-w-0">
            <code class="text-xs">{{ key.prefix }}…</code>
            <span class="ml-2">{{ key.label ?? t("admin.keys.unnamed") }}</span>
            <span class="ml-2 text-xs text-muted-foreground">
              {{
                key.last_used_at
                  ? t("admin.keys.lastUsed", { at: exact(key.last_used_at) })
                  : t("admin.keys.neverUsed")
              }}
            </span>
          </span>
          <Badge v-if="key.revoked_at" variant="muted">
            {{ t("admin.keys.revoked") }}
          </Badge>
          <Button
            v-else
            variant="ghost"
            size="sm"
            :disabled="busy"
            @click="act(() => revokeKey({ path: { id: key.id }, throwOnError: true }))"
          >
            <Trash2 />
            {{ t("admin.devices.revoke") }}
          </Button>
        </div>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("admin.accounts.title") }}</CardTitle>
        <p class="text-sm text-muted-foreground">
          {{ t("admin.accounts.description") }}
        </p>
      </CardHeader>
      <CardContent class="space-y-3">
        <div class="flex gap-2">
          <Input v-model="newAccountId" placeholder="bot-argocd" class="flex-1" />
          <Button :disabled="busy || !newAccountId.trim()" @click="addAccount">
            <Plus />
            {{ t("admin.accounts.create") }}
          </Button>
        </div>

        <div
          v-for="account in accounts"
          :key="account.id"
          class="space-y-1 border-b border-border pb-2 text-sm last:border-0"
        >
          <div class="flex items-center justify-between gap-2">
            <span class="min-w-0">
              <span class="font-medium">{{ account.display_name || account.id }}</span>
              <code class="ml-2 text-xs text-muted-foreground">{{ account.id }}</code>
              <Badge v-if="account.origin === 'managed'" variant="muted" class="ml-2">
                managed
              </Badge>
            </span>
            <Badge v-if="account.disabled_at" variant="muted">
              {{ t("admin.accounts.disabled") }}
            </Badge>
            <!-- A managed account has no button at all: the configuration is its
                 source of truth, and an API call would be undone by the next
                 reconciliation (§11). -->
            <Button
              v-else-if="account.origin !== 'managed'"
              variant="ghost"
              size="sm"
              :disabled="busy"
              @click="
                act(() => deactivateAccount({ path: { id: account.id }, throwOnError: true }))
              "
            >
              <Trash2 />
              {{ t("admin.accounts.deactivate") }}
            </Button>
            <span v-else class="text-xs text-muted-foreground">
              {{ t("admin.managedByConfig") }}
            </span>
          </div>

          <!-- The id and the creation date are what actually tell two accounts
               with the same display name apart — the name alone was the whole
               reason a duplicate went unnoticed. -->
          <div class="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
            <span v-if="account.description">{{ account.description }}</span>
            <span v-if="account.description" aria-hidden="true">·</span>
            <Tooltip>
              <span class="cursor-default">
                {{ t("admin.accounts.created", { at: ago(account.created_at) }) }}
              </span>
              <template #content>{{ exact(account.created_at) }}</template>
            </Tooltip>
            <template v-if="account.created_by">
              <span aria-hidden="true">·</span>
              <span>{{ t("admin.accounts.owner") }} <code>{{ account.created_by }}</code></span>
            </template>
          </div>
        </div>
      </CardContent>
    </Card>
  </div>
</template>
