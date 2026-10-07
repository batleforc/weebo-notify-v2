<script setup lang="ts">
/** The left zone (§10.4): channels, server-side filters (§8), and who you are. */
import {
  ChevronDown,
  ChevronRight,
  Hash,
  Inbox,
  Monitor,
  Moon,
  Sun,
  X,
} from "@lucide/vue";
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";

import type { Priority, State, Status } from "@/api/generated/types.gen";
import { useSession } from "@/auth/session";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible } from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip } from "@/components/ui/tooltip";
import {
  lookOf,
  priorityLabel,
  stateLabel,
  unreadLabel,
} from "@/lib/notification";
import { useChannels } from "@/stores/channels";
import { useConsole } from "@/stores/console";
import { emptyFilters, useInbox } from "@/stores/inbox";
import { useUi } from "@/stores/ui";
import { useUnread } from "@/stores/unread";
import Wordmark from "./Wordmark.vue";

const channels = useChannels();
const inbox = useInbox();
const route = useRoute();
const router = useRouter();
const session = useSession();
const platform = useConsole();
const ui = useUi();
const unread = useUnread();
const { t } = useI18n();

// One probe per session, at the first render that needs the answer (§10.4bis).
onMounted(() => {
  void platform.probe();
});

const STATES: State[] = ["open", "working", "closed"];
const STATUSES: Status[] = [
  "success",
  "failure",
  "warning",
  "info",
  "cancelled",
];
const PRIORITIES: Priority[] = ["min", "low", "default", "high", "urgent"];

const filters = computed(() => inbox.filters);

// The channel is where you are, not a filter: excluded here and from `clear()`.
const filtered = computed(
  () =>
    filters.value.state.length > 0 ||
    filters.value.status.length > 0 ||
    filters.value.minPriority !== null ||
    filters.value.tags.length > 0 ||
    filters.value.hasActions !== null,
);

/** From the URL: shareable, reload-proof, and correct on non-inbox routes too. */
const openChannelId = computed(() =>
  typeof route.params.channel === "string" ? route.params.channel : null,
);

// `Collapsible` models "open"; the persisted UI flag is "collapsed".
const filtersOpen = computed({
  get: () => !ui.filtersCollapsed,
  set: (value) => {
    ui.filtersCollapsed = !value;
  },
});

function toggle<T>(list: T[], value: T): T[] {
  return list.includes(value)
    ? list.filter((entry) => entry !== value)
    : [...list, value];
}

/** Channels navigate (`/c/<id>`); clicking the open one goes back to everything. */

function openChannel(id: string): void {
  void router.push(
    openChannelId.value === id
      ? { name: "inbox" }
      : { name: "channel", params: { channel: id } },
  );
}

function toggleState(state: State): void {
  void inbox.apply({
    ...filters.value,
    state: toggle(filters.value.state, state),
  });
}

function toggleStatus(status: Status): void {
  void inbox.apply({
    ...filters.value,
    status: toggle(filters.value.status, status),
  });
}

function setPriority(priority: Priority | null): void {
  void inbox.apply({
    ...filters.value,
    minPriority: filters.value.minPriority === priority ? null : priority,
  });
}

/** Toggle the "carries action buttons" filter (§2.3): off, then on, then off. */
function toggleHasActions(): void {
  void inbox.apply({
    ...filters.value,
    hasActions: filters.value.hasActions === true ? null : true,
  });
}

/** Clears filters but keeps the channel, which the URL still names. */
function clear(): void {
  void inbox.apply({ ...emptyFilters(), channels: filters.value.channels });
}

/** Freeform tags (§8, AND). Suggestions come from loaded rows: no enumeration endpoint. */
const tagInput = ref("");

const knownTags = computed(() => {
  const all = new Set<string>();
  for (const row of inbox.rows) {
    for (const tag of row.head.content.tags ?? []) {
      all.add(tag);
    }
  }
  return [...all].sort();
});

const tagSuggestions = computed(() => {
  const query = tagInput.value.trim().toLowerCase();
  if (!query) {
    return [];
  }
  return knownTags.value
    .filter((tag) => tag.includes(query) && !filters.value.tags.includes(tag))
    .slice(0, 8);
});

function addTag(raw: string): void {
  const value = raw.trim().toLowerCase();
  tagInput.value = "";
  if (!value || filters.value.tags.includes(value)) {
    return;
  }
  void inbox.apply({ ...filters.value, tags: [...filters.value.tags, value] });
}

function removeTag(tag: string): void {
  void inbox.apply({
    ...filters.value,
    tags: filters.value.tags.filter((entry) => entry !== tag),
  });
}

function onTagKeydown(event: KeyboardEvent): void {
  if (event.key === "Enter") {
    event.preventDefault();
    addTag(tagInput.value);
  }
}

const connection = computed(() => {
  switch (inbox.status) {
    case "live":
      return {
        label: t("sidebar.connection.live"),
        class: "text-status-success",
      };
    case "connecting":
    case "reconnecting":
      return {
        label: t("sidebar.connection.reconnecting"),
        class: "text-copper",
      };
    case "disconnected":
      return {
        label: t("sidebar.connection.disconnected"),
        class: "text-signal",
      };
    default:
      return { label: t("sidebar.connection.offline"), class: "text-ink-dim" };
  }
});
</script>

<template>
  <nav class="flex h-full min-h-0 flex-col text-sm" :aria-label="t('sidebar.aria')">
    <div class="flex items-center justify-between gap-2 border-b border-rule px-4 py-3">
      <RouterLink to="/" class="hover:opacity-80">
        <Wordmark />
      </RouterLink>
      <Tooltip side="right">
        <span :class="['meta inline-flex items-center gap-1.5 whitespace-nowrap', connection.class]">
          <span class="size-1.5 bg-current" aria-hidden="true" />
          {{ connection.label }}
        </span>
        <template #content>{{ t("sidebar.connection.tooltip") }}</template>
      </Tooltip>
    </div>

    <section class="min-h-0 flex-1 overflow-y-auto py-3">
      <h2 class="meta px-4 pb-2">{{ t("sidebar.channels.title") }}</h2>

      <div v-if="channels.loading" class="space-y-1 px-4">
        <Skeleton v-for="index in 3" :key="index" class="h-7 w-full" />
      </div>

      <p v-else-if="channels.error" class="px-4 text-xs text-signal">{{ channels.error }}</p>

      <p v-else-if="!channels.all.length" class="px-4 text-xs text-ink-dim">
        {{ t("sidebar.channels.empty") }}
      </p>

      <template v-else>
        <button
          type="button"
          class="bay-item"
          :aria-current="openChannelId === null ? 'page' : undefined"
          @click="router.push({ name: 'inbox' })"
        >
          <Inbox class="size-4 shrink-0" />
          <span class="truncate">{{ t("sidebar.channels.all") }}</span>
        </button>

        <button
          v-for="channel in channels.all"
          :key="channel.id"
          type="button"
          class="bay-item"
          :aria-current="openChannelId === channel.id ? 'page' : undefined"
          @click="openChannel(channel.id)"
        >
          <Hash class="size-4 shrink-0" />
          <span class="min-w-0 flex-1 truncate">{{ channel.display_name || channel.id }}</span>
          <span
            v-if="channel.origin === 'managed'"
            class="meta text-[0.625rem]"
            :title="t('sidebar.channels.managed')"
          >gitops</span>
          <span
            v-if="unread.countOf(channel.id) > 0"
            class="min-w-5 bg-signal px-1 text-center text-[0.6875rem] font-bold tabular-nums text-primary-foreground"
            :aria-label="unreadLabel(unread.countOf(channel.id))"
          >{{ unread.countOf(channel.id) }}</span>
        </button>
      </template>
    </section>

    <section class="shrink-0 border-t border-rule px-4 py-3">
      <Collapsible v-model:open="filtersOpen">
        <div class="flex w-full items-center justify-between gap-2">
          <button type="button" class="meta flex items-center gap-1 hover:text-ink">
            <ChevronDown v-if="filtersOpen" class="size-3.5" />
            <ChevronRight v-else class="size-3.5" />
            {{ t("sidebar.filters.title") }}
          </button>
          <Button v-if="filtered" variant="link" size="sm" class="h-auto p-0 text-xs" @click.stop="clear">
            {{ t("sidebar.filters.clearAll") }}
          </Button>
        </div>

        <template #content>
          <div class="space-y-3 pt-3">
            <div class="seg" role="group" :aria-label="t('sidebar.filters.title')">
              <button
                v-for="state in STATES"
                :key="state"
                type="button"
                :aria-pressed="filters.state.includes(state)"
                @click="toggleState(state)"
              >
                {{ stateLabel(state) }}
              </button>
            </div>

            <div class="seg" role="group">
              <button
                v-for="status in STATUSES"
                :key="status"
                type="button"
                :aria-pressed="filters.status.includes(status)"
                @click="toggleStatus(status)"
              >
                {{ lookOf(status).label }}
              </button>
            </div>

            <div class="seg" role="group">
              <button
                v-for="priority in PRIORITIES"
                :key="priority"
                type="button"
                :aria-pressed="filters.minPriority === priority"
                @click="setPriority(priority)"
              >
                ≥{{ priorityLabel(priority) }}
              </button>
            </div>

            <div class="seg" role="group">
              <button type="button" :aria-pressed="filters.hasActions === true" @click="toggleHasActions">
                {{ t("sidebar.filters.hasActions") }}
              </button>
            </div>

            <div class="space-y-2">
              <div v-if="filters.tags.length" class="flex flex-wrap gap-1">
                <Badge v-for="tag in filters.tags" :key="tag" variant="muted" class="gap-1">
                  {{ tag }}
                  <button
                    type="button"
                    class="hover:text-ink"
                    :aria-label="t('sidebar.filters.removeTag', { tag })"
                    @click="removeTag(tag)"
                  >
                    <X class="size-3" />
                  </button>
                </Badge>
              </div>

              <div class="relative">
                <Input
                  v-model="tagInput"
                  :placeholder="t('sidebar.filters.tagPlaceholder')"
                  class="h-8 text-xs"
                  :aria-label="t('sidebar.filters.tagAriaLabel')"
                  @keydown="onTagKeydown"
                />
                <div
                  v-if="tagSuggestions.length"
                  class="absolute z-10 mt-1 w-full border border-rule-strong bg-popover p-1 text-xs"
                >
                  <button
                    v-for="suggestion in tagSuggestions"
                    :key="suggestion"
                    type="button"
                    class="block w-full px-2 py-1.5 text-left hover:bg-ground-raised"
                    @click="addTag(suggestion)"
                  >
                    {{ suggestion }}
                  </button>
                </div>
              </div>
            </div>
          </div>
        </template>
      </Collapsible>
    </section>

    <div class="shrink-0 space-y-3 border-t border-rule px-4 py-3">
      <!-- Console shows only where the server says so (§10.4bis). -->
      <div class="seg [&>*]:basis-[calc(50%-1px)]">
        <RouterLink to="/">{{ t("sidebar.nav.inbox") }}</RouterLink>
        <RouterLink to="/settings">{{ t("sidebar.nav.settings") }}</RouterLink>
        <RouterLink to="/admin">{{ t("sidebar.nav.admin") }}</RouterLink>
        <RouterLink v-if="platform.allowed" to="/console">{{ t("sidebar.nav.platform") }}</RouterLink>
      </div>
      <div class="flex items-center justify-between gap-2">
        <span class="min-w-0 truncate text-xs text-ink-dim">{{ session.displayName }}</span>
        <div class="flex items-center gap-1">
          <Tooltip>
            <Button variant="ghost" size="icon" class="size-8" @click="ui.cycleTheme()">
              <!-- The icon is the theme a click switches *to*. -->
              <Sun v-if="ui.theme === 'dark'" />
              <Monitor v-else-if="ui.theme === 'light'" />
              <Moon v-else />
              <span class="sr-only">{{ t("sidebar.session.toggleTheme") }}</span>
            </Button>
            <template #content>{{ t("sidebar.session.themeTooltip", { theme: ui.theme }) }}</template>
          </Tooltip>
          <Button variant="ghost" size="sm" @click="session.logout()">
            {{ t("sidebar.session.logout") }}
          </Button>
        </div>
      </div>
    </div>
  </nav>
</template>
