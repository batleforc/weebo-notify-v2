<script setup lang="ts">
/** The three-zone shell (§10.4): channels, inbox, detail. Under `lg` the sidebar becomes a drawer. */
import { Inbox, Menu, Search, Settings, X } from "@lucide/vue";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppSidebar from "@/components/layout/AppSidebar.vue";
import AppToaster from "@/components/layout/AppToaster.vue";
import CommandPalette from "@/components/layout/CommandPalette.vue";
import PushPrompt from "@/components/layout/PushPrompt.vue";
import Wordmark from "@/components/layout/Wordmark.vue";
import NotificationDetail from "@/components/notification/NotificationDetail.vue";
import { Button } from "@/components/ui/button";
import { onDeepLink } from "@/lib/deeplink";
import { opensPalette } from "@/lib/palette";
import { resubscribe } from "@/lib/push";
import { register } from "@/lib/service-worker";
import { cn } from "@/lib/utils";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import { useInbox } from "@/stores/inbox";
import { usePalette } from "@/stores/palette";
import { usePreferences } from "@/stores/preferences";
import { useToasts } from "@/stores/toasts";
import { useUi } from "@/stores/ui";
import { useUnread } from "@/stores/unread";

const channels = useChannels();
const detail = useDetail();
const inbox = useInbox();
const preferences = usePreferences();
const toasts = useToasts();
const ui = useUi();
const unread = useUnread();
const route = useRoute();
const router = useRouter();
const { t } = useI18n();

const sidebarOpen = ref(false);
const palette = usePalette();

/** The routes that render InboxView: only they get a detail pane (§10.4). */
const INBOX_ROUTES = [
  "inbox",
  "notification",
  "channel",
  "channel-notification",
];

const DETAIL_ROUTES = ["notification", "channel-notification"];

/** `/c/<id>` and `/c/<id>/n/<n>` share the param: reading a notification keeps its channel. */
const routeChannel = computed(() =>
  typeof route.params.channel === "string" ? route.params.channel : null,
);

const isInbox = computed(() => INBOX_ROUTES.includes(String(route.name ?? "")));

function tabClass(active: boolean): string {
  return cn(
    "relative flex h-14 flex-col items-center justify-center gap-1 text-[0.625rem] uppercase tracking-[0.08em]",
    active ? "text-ink" : "text-ink-dim",
    active &&
      "after:absolute after:inset-x-6 after:top-0 after:h-0.5 after:bg-signal",
  );
}

/** Below `xl`, only "under" keeps the pane in the grid; left/right become a full-screen layer. */
const detailStacked = computed(
  () => isInbox.value && ui.detailPosition === "under",
);

const contentGridClass = computed(() => {
  if (!isInbox.value) {
    return "";
  }
  switch (ui.detailPosition) {
    case "left":
      return "xl:grid-cols-[26rem_minmax(0,1fr)]";
    case "under":
      // Second row only when something is selected, else small windows reserve blank space.
      return detail.id
        ? "grid-rows-[minmax(0,1fr)_22rem]"
        : "xl:grid-rows-[minmax(0,1fr)_22rem]";
    default:
      return "xl:grid-cols-[minmax(0,1fr)_26rem]";
  }
});

const mainOrderClass = computed(() =>
  isInbox.value && ui.detailPosition === "left" ? "xl:order-2" : "",
);

/** The divider follows the detail pane's side. */
const detailPositionClass = computed(() => {
  switch (ui.detailPosition) {
    case "left":
      return "xl:order-1 xl:border-l-0 xl:border-r";
    case "under":
      // Also positions the empty placeholder's divider.
      return "xl:border-l-0 xl:border-t";
    default:
      return "";
  }
});

/** On `window` so the palette opens from anywhere (§10.4). */
function onKeydown(event: KeyboardEvent): void {
  if (opensPalette(event)) {
    event.preventDefault();
    palette.toggle();
  }
}
let unsubscribe: (() => void) | null = null;
let unhandPush: (() => void) | null = null;
let unlisten: (() => void) | null = null;
let unmounted = false;

onMounted(async () => {
  window.addEventListener("keydown", onKeydown);

  // One stream per tab, shared with the detail panel (§8.2).
  unsubscribe = inbox.subscribe((event) => {
    detail.ingest(event);
    // Preferences before the toast, so a silencing change applies to it.
    preferences.ingest(event);
    toasts.arrival(event);
    // Every event kind may change the server's count; `refresh` coalesces.
    unread.refresh();
  });

  // Pushes the worker forwarded because this tab is visible (`public/sw.js`).
  const onMessage = (event: MessageEvent) => {
    const message = event.data as { type?: string; envelope?: unknown } | null;
    if (message?.type === "push" && message.envelope) {
      const envelope = message.envelope as {
        title?: string;
        closure?: boolean;
      };
      // A closure (RFC 0004) already reached this tab on the stream: no toast.
      if (!envelope.closure) {
        toasts.pushed(envelope);
      }
      // The worker shows the OS notification itself unless told this tab took
      // it — a visible tab with no shell mounted (the login page) never answers.
      event.ports[0]?.postMessage(true);
    }
  };
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.addEventListener("message", onMessage);
    unhandPush = () =>
      navigator.serviceWorker.removeEventListener("message", onMessage);
  }

  void register(() => void resubscribe());

  // Desktop `weebo-notify://n/<id>` clicks (§10bis.3quater); via the router so its guard runs.
  // Unmounted before it resolved: nobody is left to call `unlisten`, so stop now.
  void onDeepLink((path) => void router.push(path)).then((stop) => {
    if (unmounted) {
      stop?.();
    } else {
      unlisten = stop;
    }
  });

  // Stream before the page, or events published during the load are missed.
  inbox.start();
  await Promise.all([
    channels.load(),
    inbox.live().then(inbox.load),
    preferences.load(),
    unread.load(),
  ]);
});

onBeforeUnmount(() => {
  unmounted = true;
  window.removeEventListener("keydown", onKeydown);
  unsubscribe?.();
  unhandPush?.();
  unlisten?.();
  inbox.stop();
  unread.stop();
});

watch(
  () => route.fullPath,
  () => {
    sidebarOpen.value = false;
  },
);

/** Permalinks open the panel (§10.4). */
watch(
  () => [route.name, route.params.id] as const,
  ([name, id]) => {
    if (DETAIL_ROUTES.includes(String(name ?? "")) && typeof id === "string") {
      void detail.open(id);
    } else {
      detail.close();
    }
  },
  { immediate: true },
);

/** Keeps the inbox filter in sync with the route, in both directions. Non-inbox routes are left alone. */
watch(
  () => [route.name, routeChannel.value] as const,
  ([name, channel]) => {
    if (!INBOX_ROUTES.includes(String(name ?? ""))) {
      return;
    }
    const next = channel === null ? [] : [channel];
    const current = inbox.filters.channels;
    // `apply` reloads and restarts the stream; skip it when nothing changed.
    if (
      current.length === next.length &&
      current.every((c, i) => c === next[i])
    ) {
      return;
    }
    void inbox.apply({ ...inbox.filters, channels: next });
  },
  { immediate: true },
);

/** Close by going back to the channel the notification was read from. */
function closeDetail(): void {
  const channel = routeChannel.value;
  void router.push(
    channel === null
      ? { name: "inbox" }
      : { name: "channel", params: { channel } },
  );
}
</script>

<template>
  <CommandPalette />

  <div class="grid h-dvh grid-rows-[auto_minmax(0,1fr)_auto] lg:grid-rows-1">
    <!-- Narrow viewports get a bar; wide ones have the sidebar. -->
    <header
      class="safe-top flex items-center justify-between gap-2 border-b border-rule px-3 py-2 lg:hidden"
    >
      <RouterLink to="/" class="px-1"><Wordmark /></RouterLink>
      <Button variant="ghost" size="icon" @click="palette.toggle()">
        <Search />
        <span class="sr-only">{{ t("layout.search") }}</span>
      </Button>
    </header>

    <!-- Scrim: z-30 under the drawer's z-40; catches only clicks that missed it. -->
    <div
      v-if="sidebarOpen"
      class="fixed inset-0 z-30 bg-ground/80 lg:hidden"
      aria-hidden="true"
      @click="sidebarOpen = false"
    />

    <div class="grid min-h-0 lg:grid-cols-[17rem_minmax(0,1fr)]">
      <!-- `cn` (tailwind-merge) so `bg-card` really overrides `bg-card/40`. -->
      <div
        id="app-sidebar"
        :class="
          cn(
            'min-h-0 border-r border-rule bg-ground-sunk',
            sidebarOpen
              ? 'safe-top fixed inset-y-0 left-0 z-40 w-[min(20rem,85vw)] border-r-rule-strong lg:static lg:w-auto'
              : 'hidden lg:block',
          )
        "
      >
        <AppSidebar @click="sidebarOpen = false" />
      </div>

      <!-- DOM order is always list, detail; classes decide the visual arrangement. -->
      <div class="grid min-h-0" :class="contentGridClass">
        <main class="min-h-0 overflow-hidden" :class="mainOrderClass">
          <RouterView />
        </main>

        <template v-if="isInbox">
          <!-- Under xl the panel is a full-screen layer (§10.4). -->
          <div
            v-if="detail.id"
            :class="
              cn(
                'min-h-0 border-rule bg-background',
                detailStacked
                  ? 'border-t'
                  : 'safe-top fixed inset-0 z-30 border-l xl:static xl:z-auto',
                detailPositionClass,
              )
            "
          >
            <!-- The only close a loaded notification has on touch devices. -->
            <Button
              variant="outline"
              size="icon"
              class="absolute right-3 top-3 z-10 bg-ground xl:hidden"
              @click="closeDetail"
            >
              <X />
              <span class="sr-only">{{ t("layout.closeDetail") }}</span>
            </Button>
            <NotificationDetail />
          </div>
          <div
            v-else
            class="hidden min-h-0 border-l border-rule xl:block"
            :class="detailPositionClass"
          >
            <NotificationDetail />
          </div>
        </template>
      </div>
    </div>

    <!-- Phone and tablet: the four places a thumb goes, always in reach. -->
    <nav
      class="safe-bottom grid grid-cols-4 border-t border-rule-strong bg-ground-sunk lg:hidden"
      :aria-label="t('layout.tabs')"
    >
      <RouterLink to="/" :class="tabClass(isInbox)" :aria-current="isInbox ? 'page' : undefined">
        <Inbox class="size-5" />
        <span>{{ t("sidebar.nav.inbox") }}</span>
        <span
          v-if="unread.total > 0"
          class="absolute right-[calc(50%-1.5rem)] top-1.5 min-w-4 bg-signal px-1 text-center text-[0.625rem] font-bold leading-4 text-primary-foreground"
        >{{ unread.total > 99 ? "99+" : unread.total }}</span>
      </RouterLink>
      <button type="button" :class="tabClass(sidebarOpen)" :aria-expanded="sidebarOpen" aria-controls="app-sidebar" @click="sidebarOpen = !sidebarOpen">
        <Menu class="size-5" />
        <span>{{ t("layout.channels") }}</span>
      </button>
      <button type="button" :class="tabClass(false)" @click="palette.toggle()">
        <Search class="size-5" />
        <span>{{ t("layout.search") }}</span>
      </button>
      <RouterLink
        to="/settings"
        :class="tabClass(route.name === 'settings')"
        :aria-current="route.name === 'settings' ? 'page' : undefined"
      >
        <Settings class="size-5" />
        <span>{{ t("sidebar.nav.settings") }}</span>
      </RouterLink>
    </nav>
  </div>

  <!-- Outside the grid: a fixed grid child still gets an implicit row on lg+. -->
  <AppToaster />
  <PushPrompt />
</template>
