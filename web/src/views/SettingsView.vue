<script setup lang="ts">
/** Settings (§10.4, §2.4). Every profile field shows whether you own it or the IdP does, and can hand it back. */
import { KeyRound, RotateCcw, Trash2, Upload } from "@lucide/vue";
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import {
  createKey,
  listKeys,
  readProfile,
  revokeKey,
  setAvatar,
  updateProfile,
  whoami,
} from "@/api/generated";
import type {
  ApiKey,
  Profile,
  ProfileField,
  Scope,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { useSession } from "@/auth/session";
import ConnectionsPanel from "@/components/settings/ConnectionsPanel.vue";
import DndPanel from "@/components/settings/DndPanel.vue";
import PushPanel from "@/components/settings/PushPanel.vue";
import ServerPanel from "@/components/settings/ServerPanel.vue";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { avatarObjectUrl } from "@/lib/avatar";
import { isDesktop } from "@/lib/desktop";
import { exact } from "@/lib/format";
import { LOCALE_KEY, type Locale } from "@/lib/locale";
import { useChannels } from "@/stores/channels";
import { useUi } from "@/stores/ui";

const session = useSession();
const ui = useUi();
const channels = useChannels();
const { t } = useI18n();
const desktop = isDesktop();

const profile = ref<Profile | null>(null);
const loading = ref(true);
const saving = ref(false);
const error = ref<string | null>(null);
const file = ref<HTMLInputElement | null>(null);

const draft = ref({ display_name: "", timezone: "" });

const claimed = computed(
  () => new Set<ProfileField>(profile.value?.overridden ?? []),
);

/** An object URL: the avatar route needs a bearer an `<img>` can't send (§5.1, `lib/avatar`). */
const avatarUrl = ref<string | null>(null);

async function showAvatar(id: string | null | undefined): Promise<void> {
  avatarUrl.value = id ? await avatarObjectUrl(id) : null;
}

function hydrate(next: Profile): void {
  profile.value = next;
  // Every answer funnels here. Not awaited; `avatarObjectUrl` caches and never rejects.
  void showAvatar(next.avatar);
  draft.value = {
    display_name: next.display_name ?? "",
    timezone: next.timezone ?? "",
  };

  // Adopt the account's UI language once, never over a local choice (§ C5).
  const prefix = next.locale?.slice(0, 2).toLowerCase();
  if (
    !localStorage.getItem(LOCALE_KEY) &&
    (prefix === "en" || prefix === "fr")
  ) {
    ui.locale = prefix;
  }
}

async function load(): Promise<void> {
  loading.value = true;
  try {
    const answer = await readProfile({ throwOnError: true });
    hydrate(answer.data);
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    loading.value = false;
  }
}

/** Absent = leave alone, `null` = hand back to the IdP (§2.4). */
async function patch(body: Record<string, string | null>): Promise<void> {
  saving.value = true;
  error.value = null;
  try {
    const answer = await updateProfile({ body, throwOnError: true });
    hydrate(answer.data);
  } catch (caught) {
    error.value = failureOf(caught).message;
  } finally {
    saving.value = false;
  }
}

/** Send only what changed, so editing one field doesn't claim the others. */
async function save(): Promise<void> {
  if (!profile.value) {
    return;
  }
  const body: Record<string, string | null> = {};
  const current = profile.value;
  if (draft.value.display_name !== (current.display_name ?? "")) {
    body.display_name = draft.value.display_name || null;
  }
  if (draft.value.timezone !== (current.timezone ?? "")) {
    body.timezone = draft.value.timezone || null;
  }
  await patch(body);
}

/** Applied locally at once, and persisted so other devices inherit it (§ C5). */
async function setLocale(value: Locale): Promise<void> {
  ui.locale = value;
  await patch({ locale: value });
}

function release(field: "display_name" | "timezone" | "locale"): Promise<void> {
  return patch({ [field]: null });
}

async function upload(event: globalThis.Event): Promise<void> {
  const picked = (event.target as HTMLInputElement).files?.[0];
  if (!picked) {
    return;
  }

  saving.value = true;
  error.value = null;
  try {
    const answer = await setAvatar({
      body: picked,
      headers: { "content-type": "application/octet-stream" },
      throwOnError: true,
    });
    hydrate(answer.data);
  } catch (caught) {
    // The problem `code` explains size/animation/format refusals (§2.4).
    error.value = failureOf(caught).message;
  } finally {
    saving.value = false;
  }
}

/** Personal CLI keys (§9.1, §5.2): always owned by the caller, whose id needs `whoami`. */
const cliKeys = ref<ApiKey[]>([]);
const cliKeyLabel = ref(
  `notify-cli · ${new Date().toISOString().slice(0, 10)}`,
);
const cliKeyChannels = ref<Set<string>>(new Set());
const cliKeyScopes = ref<Set<Scope>>(new Set(["subscribe"]));
const cliKeyMinted = ref<string | null>(null);
const cliKeyBusy = ref(false);
const cliKeyError = ref<string | null>(null);

async function loadCliKeys(): Promise<void> {
  try {
    const answer = await listKeys({ throwOnError: true });
    // The server also returns administered service accounts' keys.
    cliKeys.value = answer.data.filter((key) => key.owner.owner === "user");
  } catch (caught) {
    cliKeyError.value = failureOf(caught).message;
  }
}

function toggle<T>(set: Set<T>, value: T): void {
  if (!set.delete(value)) {
    set.add(value);
  }
}

async function mintCliKey(): Promise<void> {
  if (cliKeyChannels.value.size === 0 || cliKeyScopes.value.size === 0) {
    return;
  }
  cliKeyBusy.value = true;
  cliKeyError.value = null;
  try {
    const me = await whoami({ throwOnError: true });
    const answer = await createKey({
      body: {
        label: cliKeyLabel.value.trim() || null,
        owner: { owner: "user", id: me.data.id },
        channels: [...cliKeyChannels.value],
        scopes: [...cliKeyScopes.value],
      },
      throwOnError: true,
    });
    // Shown once (§5.2).
    cliKeyMinted.value = answer.data.secret;
    await loadCliKeys();
  } catch (caught) {
    cliKeyError.value = failureOf(caught).message;
  } finally {
    cliKeyBusy.value = false;
  }
}

async function revokeCliKey(id: string): Promise<void> {
  cliKeyBusy.value = true;
  cliKeyError.value = null;
  try {
    await revokeKey({ path: { id }, throwOnError: true });
    await loadCliKeys();
  } catch (caught) {
    cliKeyError.value = failureOf(caught).message;
  } finally {
    cliKeyBusy.value = false;
  }
}

onMounted(() => {
  void load();
  void channels.load();
  void loadCliKeys();
});

const origin = window.location.origin;

/** The browser's own install offer, when it makes one (Chromium only). */
const installPrompt = ref<{ prompt: () => Promise<unknown> } | null>(null);
window.addEventListener("beforeinstallprompt", (event) => {
  event.preventDefault();
  installPrompt.value = event as unknown as { prompt: () => Promise<unknown> };
});

async function install(): Promise<void> {
  await installPrompt.value?.prompt();
  installPrompt.value = null;
}

const appearance = computed(() => [
  {
    label: t("settings.appearance.themeLabel"),
    current: ui.theme,
    set: (v: string) => {
      ui.theme = v as typeof ui.theme;
    },
    options: (["system", "light", "dark"] as const).map((value) => ({
      value,
      label: t(`settings.appearance.themes.${value}`),
    })),
  },
  {
    label: t("settings.appearance.densityLabel"),
    current: ui.density,
    set: (v: string) => {
      ui.density = v as typeof ui.density;
    },
    options: (["compact", "comfortable"] as const).map((value) => ({
      value,
      label: t(`settings.appearance.densities.${value}`),
    })),
  },
  {
    label: t("settings.appearance.detailLabel"),
    current: ui.detailPosition,
    set: (v: string) => {
      ui.detailPosition = v as typeof ui.detailPosition;
    },
    options: (["right", "left", "under"] as const).map((value) => ({
      value,
      label: t(`settings.appearance.detailPosition.${value}`),
    })),
  },
]);
</script>

<template>
  <div class="h-full overflow-y-auto">
    <div class="mx-auto max-w-3xl px-4 pb-16 pt-6 sm:px-8 sm:pt-10">
    <h1 class="display mb-6 uppercase">{{ t("settings.title") }}</h1>

    <p v-if="error" class="text-sm text-signal">{{ error }}</p>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("settings.profile.title") }}</CardTitle>
        <p class="text-sm text-ink-dim">
          {{ t("settings.profile.description") }}
        </p>
      </CardHeader>

      <CardContent class="space-y-4">
        <div class="flex items-center gap-4">
          <img
            v-if="avatarUrl"
            :src="avatarUrl"
            alt=""
            class="size-16 border border-rule object-cover"
          />
          <div
            v-else
            class="flex size-16 items-center justify-center border border-dashed border-rule text-xs text-ink-dim"
          >
            {{ t("settings.profile.noAvatar") }}
          </div>

          <div class="space-y-1">
            <Button variant="outline" size="sm" :disabled="saving" @click="file?.click()">
              <Upload />
              {{ t("settings.profile.changeAvatar") }}
            </Button>
            <p class="text-xs text-ink-dim">
              {{ t("settings.profile.avatarHint") }}
            </p>
          </div>
          <input
            ref="file"
            type="file"
            class="hidden"
            accept="image/png,image/jpeg,image/webp"
            @change="upload"
          />
        </div>

        <Separator />

        <label class="block space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            {{ t("settings.profile.displayName") }}
            <Badge v-if="claimed.has('display_name')" variant="secondary">
              {{ t("settings.profile.claimed") }}
            </Badge>
            <Badge v-else variant="muted">{{ t("settings.profile.followsIdp") }}</Badge>
            <Button
              v-if="claimed.has('display_name')"
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-xs"
              @click="release('display_name')"
            >
              <RotateCcw class="size-3" />
              {{ t("settings.profile.releaseToIdp") }}
            </Button>
          </span>
          <Input v-model="draft.display_name" :disabled="loading || saving" />
        </label>

        <label class="block space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            {{ t("settings.profile.timezone") }}
            <Badge v-if="claimed.has('timezone')" variant="secondary">
              {{ t("settings.profile.claimed") }}
            </Badge>
            <Button
              v-if="claimed.has('timezone')"
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-xs"
              @click="release('timezone')"
            >
              <RotateCcw class="size-3" />
              {{ t("settings.profile.releaseToIdp") }}
            </Button>
          </span>
          <Input
            v-model="draft.timezone"
            placeholder="Europe/Paris"
            :disabled="loading || saving"
          />
          <span class="text-xs text-ink-dim">
            {{ t("settings.profile.timezoneHint") }}
          </span>
        </label>

        <div class="space-y-1">
          <span class="flex items-center gap-2 text-sm font-medium">
            {{ t("settings.locale.title") }}
            <Badge v-if="claimed.has('locale')" variant="secondary">
              {{ t("settings.profile.claimed") }}
            </Badge>
            <Badge v-else variant="muted">{{ t("settings.profile.followsIdp") }}</Badge>
            <Button
              v-if="claimed.has('locale')"
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-xs"
              @click="release('locale')"
            >
              <RotateCcw class="size-3" />
              {{ t("settings.profile.releaseToIdp") }}
            </Button>
          </span>
          <div class="seg max-w-xs" role="group" :aria-label="t('settings.locale.title')">
            <button type="button" :aria-pressed="ui.locale === 'fr'" :disabled="loading || saving" @click="setLocale('fr')">
              {{ t("settings.locale.fr") }}
            </button>
            <button type="button" :aria-pressed="ui.locale === 'en'" :disabled="loading || saving" @click="setLocale('en')">
              {{ t("settings.locale.en") }}
            </button>
          </div>
        </div>

        <Button :disabled="loading || saving" @click="save">
          {{ saving ? t("settings.profile.saving") : t("settings.profile.save") }}
        </Button>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("settings.appearance.title") }}</CardTitle>
      </CardHeader>
      <CardContent class="space-y-4">
        <div v-for="group in appearance" :key="group.label" class="grid gap-2 sm:grid-cols-[10rem_1fr] sm:items-center">
          <span class="meta">{{ group.label }}</span>
          <div class="seg" role="group" :aria-label="group.label">
            <button
              v-for="option in group.options"
              :key="option.value"
              type="button"
              :aria-pressed="group.current === option.value"
              @click="group.set(option.value)"
            >
              {{ option.label }}
            </button>
          </div>
        </div>
        <p class="text-xs text-ink-dim">{{ t("settings.appearance.detailHint") }}</p>
      </CardContent>
    </Card>

    <!-- §10.4 "Apps": the other clients, and how to reach them from here. -->
    <Card>
      <CardHeader>
        <CardTitle>{{ t("settings.apps.title") }}</CardTitle>
        <p class="max-w-[64ch] text-sm text-ink-dim">{{ t("settings.apps.description") }}</p>
      </CardHeader>
      <CardContent class="space-y-3 text-sm">
        <p class="meta">{{ t("settings.apps.cli") }}</p>
        <code class="block select-all border border-rule bg-ground-sunk p-3 text-xs [overflow-wrap:anywhere]">notify login --web-url {{ origin }}</code>
        <template v-if="installPrompt">
          <p class="meta pt-2">{{ t("settings.apps.pwa") }}</p>
          <Button variant="outline" size="sm" @click="install">{{ t("settings.apps.install") }}</Button>
        </template>
      </CardContent>
    </Card>

    <!-- Web Push is a browser's; the desktop shell notifies natively (§10bis). -->
    <ConnectionsPanel />

    <PushPanel v-if="!desktop" />
    <ServerPanel v-if="desktop" />

    <DndPanel />

    <Card>
      <CardHeader>
        <CardTitle>{{ t("settings.keys.title") }}</CardTitle>
        <p class="text-sm text-ink-dim">
          {{ t("settings.keys.description") }}
        </p>
      </CardHeader>
      <CardContent class="space-y-3">
        <p v-if="cliKeyError" class="text-sm text-signal">{{ cliKeyError }}</p>

        <!-- Shown once: only the hash is kept (§5.2). -->
        <Card v-if="cliKeyMinted" class="border border-copper p-4">
          <CardContent class="space-y-2 pt-4">
            <p class="text-sm text-ink-dim">
              {{ t("settings.keys.mintedHint") }}
            </p>
            <code class="block select-all border border-rule bg-ground-sunk p-3 text-xs [overflow-wrap:anywhere]">{{
              cliKeyMinted
            }}</code>
            <Button variant="outline" size="sm" @click="cliKeyMinted = null">
              {{ t("settings.keys.copied") }}
            </Button>
          </CardContent>
        </Card>

        <template v-else>
          <Input v-model="cliKeyLabel" :placeholder="t('settings.keys.labelPlaceholder')" />

          <div class="space-y-1">
            <p class="text-xs font-medium text-ink-dim">
              {{ t("settings.keys.channels") }}
            </p>
            <label
              v-for="channel in channels.all"
              :key="channel.id"
              class="flex items-center gap-2 text-sm"
            >
              <input
                type="checkbox"
                :checked="cliKeyChannels.has(channel.id)"
                @change="toggle(cliKeyChannels, channel.id)"
              />
              {{ channel.display_name || channel.id }}
            </label>
            <p v-if="!channels.all.length" class="text-xs text-ink-dim">
              {{ t("settings.keys.noChannels") }}
            </p>
          </div>

          <div class="space-y-1">
            <p class="text-xs font-medium text-ink-dim">
              {{ t("settings.keys.scope") }}
            </p>
            <label
              v-for="scope in (['subscribe', 'publish', 'manage'] as Scope[])"
              :key="scope"
              class="flex items-center gap-2 text-sm"
            >
              <input
                type="checkbox"
                :checked="cliKeyScopes.has(scope)"
                @change="toggle(cliKeyScopes, scope)"
              />
              {{ scope }}
            </label>
          </div>

          <Button
            :disabled="cliKeyBusy || !cliKeyChannels.size || !cliKeyScopes.size"
            @click="mintCliKey"
          >
            <KeyRound />
            {{ t("settings.keys.create") }}
          </Button>
        </template>

        <div
          v-for="key in cliKeys"
          :key="key.id"
          class="rule-row flex flex-wrap items-center justify-between gap-2 py-2 text-sm last:border-0"
        >
          <span class="min-w-0">
            <code class="text-xs">{{ key.prefix }}…</code>
            <span class="ml-2">{{ key.label ?? t("settings.keys.unnamed") }}</span>
            <span class="ml-2 text-xs text-ink-dim">
              {{
                key.last_used_at
                  ? t("settings.keys.lastUsed", { at: exact(key.last_used_at) })
                  : t("settings.keys.neverUsed")
              }}
            </span>
          </span>
          <Badge v-if="key.revoked_at" variant="muted">
            {{ t("settings.keys.revoked") }}
          </Badge>
          <Button
            v-else
            variant="ghost"
            size="sm"
            :disabled="cliKeyBusy"
            @click="revokeCliKey(key.id)"
          >
            <Trash2 />
            {{ t("settings.keys.revoke") }}
          </Button>
        </div>
      </CardContent>
    </Card>

    <Card>
      <CardHeader>
        <CardTitle>{{ t("settings.session.title") }}</CardTitle>
      </CardHeader>
      <CardContent class="space-y-2 text-sm">
        <p class="text-ink-dim">
          {{ t("settings.session.connectedAs", { name: session.displayName }) }}
        </p>
        <p v-if="session.groups.length" class="text-xs text-ink-dim">
          {{ t("settings.session.groups", { groups: session.groups.join(", ") }) }}
        </p>
        <Button variant="outline" size="sm" @click="session.logout()">
          {{ t("settings.session.logout") }}
        </Button>
      </CardContent>
    </Card>
    </div>
  </div>
</template>
