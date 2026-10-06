<script setup lang="ts">
/**
 * Channels: create, tune, share, delete (§10.4, §8).
 *
 * A managed channel gets no controls: config owns it (§11) and an API call would
 * be undone at the next reconciliation. Authorisation is the server's call — the
 * screen offers the controls and renders the refusal (§5.1).
 */
import { ChevronDown, ChevronRight, Plus, Trash2 } from "@lucide/vue";
import { ref } from "vue";
import { useI18n } from "vue-i18n";

import {
  addMember,
  createChannel,
  deleteChannel,
  removeMember,
  updateChannel,
} from "@/api/generated";
import type { Channel, Principal, Scope } from "@/api/generated/types.gen";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useChannels } from "@/stores/channels";

const props = defineProps<{ busy: boolean }>();
const emit = defineEmits<{ act: [work: () => Promise<unknown>] }>();

const channels = useChannels();
const { t } = useI18n();

const SCOPES: Scope[] = ["subscribe", "publish", "manage"];

const open = ref<string | null>(null);
const newId = ref("");
const newName = ref("");
const ttl = ref({ default_ttl: "", max_ttl: "" });
const member = ref<{
  kind: "user" | "service_account";
  id: string;
  scopes: Scope[];
}>({
  kind: "user",
  id: "",
  scopes: ["subscribe"],
});

function toggle(channel: Channel): void {
  open.value = open.value === channel.id ? null : channel.id;
  ttl.value = {
    default_ttl: channel.policy.ttl.default,
    max_ttl: channel.policy.ttl.max ?? "",
  };
}

function principalLabel(principal: Principal): string {
  return "id" in principal
    ? `${principal.kind}:${principal.id}`
    : principal.kind;
}

/** The route takes the bare id: a `usr_…` is a user, anything else a bot. */
function principalPath(principal: Principal): string {
  return "id" in principal ? principal.id : principal.kind;
}

function create(): void {
  const id = newId.value.trim();
  if (!id) return;
  emit("act", async () => {
    await createChannel({
      body: { id, display_name: newName.value.trim() || null },
      throwOnError: true,
    });
    newId.value = "";
    newName.value = "";
    open.value = id;
  });
}

/** Only the knobs that moved: restating the rest is how two admins collide. */
function savePolicy(channel: Channel): void {
  const body: Record<string, string> = {};
  if (
    ttl.value.default_ttl &&
    ttl.value.default_ttl !== channel.policy.ttl.default
  ) {
    body.default_ttl = ttl.value.default_ttl;
  }
  if (ttl.value.max_ttl !== (channel.policy.ttl.max ?? "")) {
    body.max_ttl = ttl.value.max_ttl || "0"; // `0` lifts the ceiling (§7.2bis)
  }
  if (Object.keys(body).length) {
    emit("act", () =>
      updateChannel({
        path: { channel: channel.id },
        body,
        throwOnError: true,
      }),
    );
  }
}

function toggleReceipts(channel: Channel): void {
  emit("act", () =>
    updateChannel({
      path: { channel: channel.id },
      body: { receipts_enabled: !channel.policy.receipts_enabled },
      throwOnError: true,
    }),
  );
}

function toggleScope(scope: Scope): void {
  const scopes = member.value.scopes;
  member.value.scopes = scopes.includes(scope)
    ? scopes.filter((s) => s !== scope)
    : [...scopes, scope];
}

function add(channel: Channel): void {
  const id = member.value.id.trim();
  if (!id || !member.value.scopes.length) return;
  emit("act", async () => {
    await addMember({
      path: { channel: channel.id },
      body: {
        principal: { kind: member.value.kind, id } as Principal,
        scopes: member.value.scopes,
      },
      throwOnError: true,
    });
    member.value.id = "";
  });
}

function remove(channel: Channel, principal: Principal): void {
  emit("act", () =>
    removeMember({
      path: { channel: channel.id, principal: principalPath(principal) },
      throwOnError: true,
    }),
  );
}

function destroy(channel: Channel): void {
  if (!window.confirm(t("admin.channels.deleteConfirm", { id: channel.id })))
    return;
  emit("act", () =>
    deleteChannel({ path: { channel: channel.id }, throwOnError: true }),
  );
}
</script>

<template>
  <div class="space-y-4">
    <form class="grid gap-2 sm:grid-cols-[1fr_1fr_auto]" @submit.prevent="create">
      <Input v-model="newId" placeholder="ci-infra" :aria-label="t('admin.channels.id')" />
      <Input v-model="newName" :placeholder="t('admin.channels.namePlaceholder')" :aria-label="t('admin.channels.name')" />
      <Button type="submit" :disabled="props.busy || !newId.trim()">
        <Plus />
        {{ t("admin.channels.create") }}
      </Button>
    </form>

    <p v-if="!channels.all.length" class="text-sm text-ink-dim">{{ t("admin.channels.empty") }}</p>

    <ul class="border-t border-rule">
      <li v-for="channel in channels.all" :key="channel.id" class="rule-row">
        <button
          type="button"
          class="flex w-full items-center gap-3 py-3 text-left hover:text-ink"
          :aria-expanded="open === channel.id"
          @click="toggle(channel)"
        >
          <ChevronDown v-if="open === channel.id" class="size-4 shrink-0 text-ink-dim" />
          <ChevronRight v-else class="size-4 shrink-0 text-ink-dim" />
          <span class="min-w-0 flex-1">
            <span class="block truncate">{{ channel.display_name || channel.id }}</span>
            <code class="block truncate text-xs text-ink-dim">{{ channel.id }}</code>
          </span>
          <Badge v-if="channel.origin === 'managed'" variant="outline">gitops</Badge>
          <span class="meta hidden tabular-nums sm:inline">
            {{ t("admin.channels.memberCount", { count: channel.members.length }, channel.members.length) }}
          </span>
        </button>

        <div v-if="open === channel.id" class="space-y-5 pb-5 pl-7">
          <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-1 text-xs">
            <dt class="meta">TTL</dt>
            <dd>{{ channel.policy.ttl.default }}<template v-if="channel.policy.ttl.max"> / max {{ channel.policy.ttl.max }}</template></dd>
            <dt class="meta">{{ t("admin.channels.closeAfter") }}</dt>
            <dd>{{ channel.policy.close_after }}</dd>
            <dt class="meta">{{ t("admin.channels.receipts") }}</dt>
            <dd>{{ channel.policy.receipts_enabled ? t("console.yes") : t("console.no") }}</dd>
            <template v-for="binding in channel.group_access ?? []" :key="binding.group">
              <dt class="meta">group</dt>
              <dd>{{ binding.group }} — {{ binding.scopes.join(", ") }}</dd>
            </template>
          </dl>

          <p v-if="channel.origin === 'managed'" class="text-xs text-ink-dim">{{ t("admin.managedByConfig") }}</p>

          <template v-else>
            <div class="space-y-2">
              <p class="meta">{{ t("admin.channels.retention") }}</p>
              <div class="grid gap-2 sm:grid-cols-[1fr_1fr_auto]">
                <Input v-model="ttl.default_ttl" :placeholder="t('admin.channels.defaultTtlPlaceholder')" />
                <Input v-model="ttl.max_ttl" :placeholder="t('admin.channels.maxTtlPlaceholder')" />
                <Button variant="outline" :disabled="props.busy" @click="savePolicy(channel)">
                  {{ t("settings.profile.save") }}
                </Button>
              </div>
              <Button variant="outline" size="sm" :aria-pressed="channel.policy.receipts_enabled" :disabled="props.busy" @click="toggleReceipts(channel)">
                {{ channel.policy.receipts_enabled ? t("admin.channels.receiptsOn") : t("admin.channels.receiptsOff") }}
              </Button>
            </div>

            <div class="space-y-2">
              <p class="meta">{{ t("admin.channels.members") }}</p>
              <ul v-if="channel.members.length" class="text-sm">
                <li v-for="m in channel.members" :key="principalLabel(m.principal)" class="rule-row flex flex-wrap items-center gap-2 py-2">
                  <code class="min-w-0 flex-1 truncate text-xs">{{ principalLabel(m.principal) }}</code>
                  <span class="meta">{{ m.scopes.join(" · ") }}</span>
                  <Button variant="ghost" size="sm" :disabled="props.busy" @click="remove(channel, m.principal)">
                    <Trash2 />
                    <span class="sr-only sm:not-sr-only">{{ t("admin.channels.removeMember") }}</span>
                  </Button>
                </li>
              </ul>
              <form class="space-y-2" @submit.prevent="add(channel)">
                <div class="grid gap-2 sm:grid-cols-[auto_1fr]">
                  <div class="seg" role="group">
                    <button type="button" :aria-pressed="member.kind === 'user'" @click="member.kind = 'user'">user</button>
                    <button type="button" :aria-pressed="member.kind === 'service_account'" @click="member.kind = 'service_account'">bot</button>
                  </div>
                  <Input v-model="member.id" :placeholder="member.kind === 'user' ? 'usr_…' : 'bot-argocd'" :aria-label="t('admin.channels.memberId')" />
                </div>
                <div class="flex flex-wrap items-center gap-2">
                  <div class="seg" role="group" :aria-label="t('settings.keys.scope')">
                    <button v-for="scope in SCOPES" :key="scope" type="button" :aria-pressed="member.scopes.includes(scope)" @click="toggleScope(scope)">
                      {{ scope }}
                    </button>
                  </div>
                  <Button type="submit" variant="outline" :disabled="props.busy || !member.id.trim() || !member.scopes.length">
                    <Plus />
                    {{ t("admin.channels.addMember") }}
                  </Button>
                </div>
              </form>
            </div>

            <Button variant="destructive" size="sm" :disabled="props.busy" @click="destroy(channel)">
              <Trash2 />
              {{ t("admin.channels.delete") }}
            </Button>
          </template>
        </div>
      </li>
    </ul>
  </div>
</template>
