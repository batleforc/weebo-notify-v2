<script setup lang="ts">
/**
 * The left zone of the shell (§10.4): channels, filters, and who you are.
 *
 * Filters are applied by asking the server (`GET /notifications` takes them all,
 * §8) rather than by hiding rows locally — a filtered inbox has to be able to page
 * through everything that matches, not through the first fifty rows that happened
 * to be loaded.
 */
import {
  ChevronDown,
  ChevronRight,
  Hash,
  Monitor,
  Moon,
  MousePointerClick,
  Radio,
  Sun,
  WifiOff,
  X,
} from "@lucide/vue";
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import type { Priority, State, Status } from "@/api/generated/types.gen";
import { useSession } from "@/auth/session";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible } from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
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

const channels = useChannels();
const inbox = useInbox();
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

const filtered = computed(
  () =>
    filters.value.channels.length > 0 ||
    filters.value.state.length > 0 ||
    filters.value.status.length > 0 ||
    filters.value.minPriority !== null ||
    filters.value.tags.length > 0 ||
    filters.value.hasActions !== null,
);

// `Collapsible`'s model is "is the content shown"; `ui.filtersCollapsed` is the
// inverse (and the one that persists), so this bridges the two without either
// side having to know about the other's sign.
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

function toggleChannel(id: string): void {
  void inbox.apply({
    ...filters.value,
    channels: toggle(filters.value.channels, id),
  });
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

function clear(): void {
  void inbox.apply(emptyFilters());
}

/**
 * Freeform tag filter (§8 AND semantics). No enumeration endpoint exists, so
 * suggestions are drawn from tags already carried by loaded rows — a hint, not
 * an authoritative list.
 */
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
        icon: Radio,
        label: t("sidebar.connection.live"),
        class: "text-status-success",
      };
    case "connecting":
    case "reconnecting":
      return {
        icon: Radio,
        label: t("sidebar.connection.reconnecting"),
        class: "text-status-warning animate-pulse",
      };
    default:
      return {
        icon: WifiOff,
        label: t("sidebar.connection.offline"),
        class: "text-muted-foreground",
      };
  }
});
</script>

<template>
  <nav
    class="flex h-full min-h-0 flex-col gap-3 p-3 text-sm"
    :aria-label="t('sidebar.aria')"
  >
    <div class="cyber-grid -m-3 mb-0 flex items-center justify-between gap-2 border-b p-3">
      <RouterLink
        to="/"
        class="font-mono text-sm font-semibold tracking-tight hover:opacity-80"
      >
        weebo<span class="cyber-text-glow text-primary">-notify</span>
      </RouterLink>
      <Tooltip side="right">
        <span :class="['flex items-center gap-1 text-xs', connection.class]">
          <component :is="connection.icon" class="size-3.5" />
          <span class="sr-only sm:not-sr-only">{{ connection.label }}</span>
        </span>
        <template #content>
          {{ t("sidebar.connection.tooltip") }}
        </template>
      </Tooltip>
    </div>

    <Separator />

    <section class="min-h-0 flex-1 space-y-1 overflow-y-auto">
      <h2 class="px-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        {{ t("sidebar.channels.title") }}
      </h2>

      <div v-if="channels.loading" class="space-y-1 p-1">
        <Skeleton v-for="index in 3" :key="index" class="h-7 w-full" />
      </div>

      <p v-else-if="channels.error" class="px-1 text-xs text-destructive">
        {{ channels.error }}
      </p>

      <p
        v-else-if="!channels.all.length"
        class="px-1 text-xs text-muted-foreground"
      >
        {{ t("sidebar.channels.empty") }}
      </p>

      <Button
        v-for="channel in channels.all"
        :key="channel.id"
        :variant="filters.channels.includes(channel.id) ? 'secondary' : 'ghost'"
        size="sm"
        class="w-full justify-start gap-2 font-normal"
        :aria-pressed="filters.channels.includes(channel.id)"
        @click="toggleChannel(channel.id)"
      >
        <Hash class="text-muted-foreground" />
        <span class="truncate">{{ channel.display_name || channel.id }}</span>
        <Badge v-if="channel.origin === 'managed'" variant="muted" class="ml-auto">
          managed
        </Badge>
        <!--
          A number and not a dot: §10.3's "jamais la couleur seule" applies here
          too, and "3 non lues" is what a screen reader must say — the digits alone
          would be read as a bare number next to a channel name.
          `ml-auto` only when no `managed` badge already took it, so the two never
          fight over the right edge.
        -->
        <Badge
          v-if="unread.countOf(channel.id) > 0"
          variant="default"
          :class="channel.origin === 'managed' ? '' : 'ml-auto'"
          :aria-label="unreadLabel(unread.countOf(channel.id))"
        >
          {{ unread.countOf(channel.id) }}
        </Badge>
      </Button>
    </section>

    <Separator />

    <section class="shrink-0 space-y-2">
      <Collapsible v-model:open="filtersOpen">
        <div class="flex w-full items-center justify-between gap-2 px-1">
          <button
            type="button"
            class="flex items-center gap-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground hover:text-foreground"
          >
            <ChevronDown v-if="filtersOpen" class="size-3.5" />
            <ChevronRight v-else class="size-3.5" />
            {{ t("sidebar.filters.title") }}
          </button>
          <Button
            v-if="filtered"
            variant="link"
            size="sm"
            class="h-auto p-0 text-xs"
            @click.stop="clear"
          >
            {{ t("sidebar.filters.clearAll") }}
          </Button>
        </div>

        <template #content>
          <div class="space-y-2 pt-2">
            <div class="flex flex-wrap gap-1 px-1">
              <Button
                v-for="state in STATES"
                :key="state"
                :variant="filters.state.includes(state) ? 'secondary' : 'outline'"
                size="sm"
                class="h-6 px-2 text-xs font-normal"
                :aria-pressed="filters.state.includes(state)"
                @click="toggleState(state)"
              >
                {{ stateLabel(state) }}
              </Button>
            </div>

            <div class="flex flex-wrap gap-1 px-1">
              <Button
                v-for="status in STATUSES"
                :key="status"
                :variant="filters.status.includes(status) ? 'secondary' : 'outline'"
                size="sm"
                :class="[
                  'h-6 gap-1 px-2 text-xs font-normal',
                  filters.status.includes(status) ? lookOf(status).color : '',
                ]"
                :aria-pressed="filters.status.includes(status)"
                @click="toggleStatus(status)"
              >
                <component :is="lookOf(status).icon" class="size-3" />
                {{ lookOf(status).label }}
              </Button>
            </div>

            <div class="flex flex-wrap gap-1 px-1">
              <Button
                v-for="priority in PRIORITIES"
                :key="priority"
                :variant="filters.minPriority === priority ? 'secondary' : 'outline'"
                size="sm"
                class="h-6 px-2 text-xs font-normal"
                :aria-pressed="filters.minPriority === priority"
                @click="setPriority(priority)"
              >
                ≥ {{ priorityLabel(priority) }}
              </Button>
            </div>

            <div class="flex flex-wrap gap-1 px-1">
              <Button
                :variant="filters.hasActions === true ? 'secondary' : 'outline'"
                size="sm"
                class="h-6 gap-1 px-2 text-xs font-normal"
                :aria-pressed="filters.hasActions === true"
                @click="toggleHasActions"
              >
                <MousePointerClick class="size-3" />
                {{ t("sidebar.filters.hasActions") }}
              </Button>
            </div>

            <div class="space-y-1 px-1">
              <div v-if="filters.tags.length" class="flex flex-wrap gap-1">
                <Badge
                  v-for="tag in filters.tags"
                  :key="tag"
                  variant="muted"
                  class="gap-1 font-normal"
                >
                  {{ tag }}
                  <button
                    type="button"
                    class="rounded-sm hover:text-foreground"
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
                  class="h-7 text-xs"
                  :aria-label="t('sidebar.filters.tagAriaLabel')"
                  @keydown="onTagKeydown"
                />
                <div
                  v-if="tagSuggestions.length"
                  class="absolute z-10 mt-1 w-full rounded-md border border-border bg-popover p-1 text-xs shadow-md"
                >
                  <button
                    v-for="suggestion in tagSuggestions"
                    :key="suggestion"
                    type="button"
                    class="block w-full rounded-sm px-2 py-1 text-left hover:bg-accent"
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

    <div class="shrink-0 space-y-2">
      <Separator />
      <div class="flex flex-wrap items-center gap-1 px-1">
        <RouterLink
          to="/settings"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          {{ t("sidebar.nav.settings") }}
        </RouterLink>
        <RouterLink
          to="/admin"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          {{ t("sidebar.nav.admin") }}
        </RouterLink>
        <!--
          Shown only where the server says so (§10.4bis). The answer comes from one
          probe rather than from a flag on the session: the role is derived per
          request from the token's groups (§14.2), so the API is the only thing that
          can answer, and a copy kept here could drift from it.
        -->
        <RouterLink
          v-if="platform.allowed"
          to="/console"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          {{ t("sidebar.nav.platform") }}
        </RouterLink>
        <RouterLink
          to="/"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          {{ t("sidebar.nav.inbox") }}
        </RouterLink>
      </div>
      <div class="flex items-center justify-between gap-2 px-1">
        <span class="min-w-0 truncate text-xs text-muted-foreground">
          {{ session.displayName }}
        </span>
        <div class="flex items-center gap-1">
          <Tooltip>
            <Button variant="ghost" size="icon" class="size-7" @click="ui.cycleTheme()">
              <!-- Each icon is the theme a click switches *to*, matching the
                   dark → light → system → dark order `cycleTheme` follows. -->
              <Sun v-if="ui.theme === 'dark'" class="size-3.5" />
              <Monitor v-else-if="ui.theme === 'light'" class="size-3.5" />
              <Moon v-else class="size-3.5" />
              <span class="sr-only">{{ t("sidebar.session.toggleTheme") }}</span>
            </Button>
            <template #content>
              {{ t("sidebar.session.themeTooltip", { theme: ui.theme }) }}
            </template>
          </Tooltip>
          <Button
            variant="ghost"
            size="sm"
            class="h-7 px-2 text-xs"
            @click="session.logout()"
          >
            {{ t("sidebar.session.logout") }}
          </Button>
        </div>
      </div>
    </div>
  </nav>
</template>
