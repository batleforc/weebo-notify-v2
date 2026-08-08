<script setup lang="ts">
/**
 * The left zone of the shell (§10.4): channels, filters, and who you are.
 *
 * Filters are applied by asking the server (`GET /notifications` takes them all,
 * §8) rather than by hiding rows locally — a filtered inbox has to be able to page
 * through everything that matches, not through the first fifty rows that happened
 * to be loaded.
 */
import { Hash, Monitor, Moon, Radio, Sun, WifiOff } from "lucide-vue-next";
import { computed, onMounted } from "vue";

import type { Priority, State, Status } from "@/api/generated/types.gen";
import { useSession } from "@/auth/session";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
    filters.value.tag !== null,
);

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

function clear(): void {
  void inbox.apply(emptyFilters());
}

const connection = computed(() => {
  switch (inbox.status) {
    case "live":
      return { icon: Radio, label: "temps réel", class: "text-status-success" };
    case "connecting":
    case "reconnecting":
      return {
        icon: Radio,
        label: "reconnexion…",
        class: "text-status-warning animate-pulse",
      };
    default:
      return {
        icon: WifiOff,
        label: "hors ligne",
        class: "text-muted-foreground",
      };
  }
});
</script>

<template>
  <nav
    class="flex h-full min-h-0 flex-col gap-3 overflow-y-auto p-3 text-sm"
    aria-label="Canaux et filtres"
  >
    <div class="cyber-grid -m-3 mb-0 flex items-center justify-between gap-2 border-b p-3">
      <span class="font-mono text-sm font-semibold tracking-tight">
        weebo<span class="cyber-text-glow text-primary">-notify</span>
      </span>
      <Tooltip side="right">
        <span :class="['flex items-center gap-1 text-xs', connection.class]">
          <component :is="connection.icon" class="size-3.5" />
          <span class="sr-only sm:not-sr-only">{{ connection.label }}</span>
        </span>
        <template #content>
          Flux SSE — reprise par curseur à la reconnexion (§8.1).
        </template>
      </Tooltip>
    </div>

    <Separator />

    <section class="space-y-1">
      <h2 class="px-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        Canaux
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
        Aucun canal. L'accès vient d'un groupe de ton fournisseur d'identité ou
        d'une invitation (§5.1).
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

    <section class="space-y-2">
      <div class="flex items-center justify-between px-1">
        <h2 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          Filtres
        </h2>
        <Button
          v-if="filtered"
          variant="link"
          size="sm"
          class="h-auto p-0 text-xs"
          @click="clear"
        >
          tout effacer
        </Button>
      </div>

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
    </section>

    <div class="mt-auto space-y-2">
      <Separator />
      <div class="flex flex-wrap items-center gap-1 px-1">
        <RouterLink
          to="/settings"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          Réglages
        </RouterLink>
        <RouterLink
          to="/admin"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          Administration
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
          Plateforme
        </RouterLink>
        <RouterLink
          to="/"
          class="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
        >
          Inbox
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
              <span class="sr-only">Changer de thème</span>
            </Button>
            <template #content>Thème : {{ ui.theme }}</template>
          </Tooltip>
          <Button
            variant="ghost"
            size="sm"
            class="h-7 px-2 text-xs"
            @click="session.logout()"
          >
            Se déconnecter
          </Button>
        </div>
      </div>
    </div>
  </nav>
</template>
