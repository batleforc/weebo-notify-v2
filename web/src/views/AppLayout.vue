<script setup lang="ts">
/**
 * The three-zone shell (§10.4): channels, inbox, detail.
 *
 * The same component serves the browser, and later the desktop and mobile shells
 * (§10bis) — what changes is the breakpoint, not the code. Under `lg` the sidebar
 * folds away behind a button and the detail takes the whole screen when something
 * is open, which is the mobile behaviour §10.4 asks for.
 */
import { Menu, X } from "lucide-vue-next";
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppSidebar from "@/components/layout/AppSidebar.vue";
import AppToaster from "@/components/layout/AppToaster.vue";
import CommandPalette from "@/components/layout/CommandPalette.vue";
import PushPrompt from "@/components/layout/PushPrompt.vue";
import NotificationDetail from "@/components/notification/NotificationDetail.vue";
import { Button } from "@/components/ui/button";
import { onDeepLink } from "@/lib/deeplink";
import { opensPalette } from "@/lib/palette";
import { resubscribe } from "@/lib/push";
import { register } from "@/lib/service-worker";
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

/**
 * Whether this route has a detail panel at all. Only the three that render
 * `InboxView` do (§10.4) — `settings`/`admin`/`console` are full pages of their
 * own, and reserving 22rem for an always-empty panel on them is not a smaller
 * version of the three-zone shell, it is 22rem taken from a page that never had
 * anything to put there. `under` is what makes this worth naming: on those
 * routes it used to leave `<main>` visibly short of the viewport, by exactly the
 * height the empty panel still claimed off-screen.
 */
const showDetailPane = computed(() =>
  ["inbox", "notification", "channel"].includes(String(route.name ?? "")),
);

/**
 * Where the list and the detail panel sit, above `xl` (§10.4, the preference in
 * Réglages › Apparence). Below `xl` the panel is always the full-screen layer it
 * already was — this only reshapes the desktop three-zone arrangement.
 *
 * One grid, not three duplicated templates: `content` always holds exactly the
 * list and the detail panel, in that DOM order, and these classes are the only
 * thing that decides whether that reads left-to-right, right-to-left, or
 * top-to-bottom. `order-*` moves them without touching the router or the
 * detail panel's own mount/unmount timing.
 */
const contentGridClass = computed(() => {
  if (!showDetailPane.value) {
    return "";
  }
  switch (ui.detailPosition) {
    case "left":
      return "xl:grid-cols-[26rem_minmax(0,1fr)]";
    case "under":
      return "xl:grid-rows-[minmax(0,1fr)_22rem]";
    default:
      return "xl:grid-cols-[minmax(0,1fr)_26rem]";
  }
});

const mainOrderClass = computed(() =>
  showDetailPane.value && ui.detailPosition === "left" ? "xl:order-2" : "",
);

/** The one border between the two panes — its edge follows whichever side the
 * detail panel ends up on, so the list is never the one carrying it. */
const detailPositionClass = computed(() => {
  switch (ui.detailPosition) {
    case "left":
      return "xl:order-1 xl:border-l-0 xl:border-r";
    case "under":
      return "xl:border-l-0 xl:border-t";
    default:
      return "";
  }
});

/**
 * The one shortcut the shell owns (§10.4).
 *
 * On `window` rather than on a focused element, because the palette has to open
 * from anywhere — including from inside the inbox list, which handles its own keys.
 * It does not fire while the palette is open: Reka UI traps focus there, and
 * `Esc` is what closes it.
 */
function onKeydown(event: KeyboardEvent): void {
  if (opensPalette(event)) {
    event.preventDefault();
    palette.toggle();
  }
}
let unsubscribe: (() => void) | null = null;
let unhandPush: (() => void) | null = null;
let unlisten: (() => void) | null = null;

onMounted(async () => {
  window.addEventListener("keydown", onKeydown);

  // The detail panel does not hold a stream of its own: one connection per tab,
  // out of the twenty a user gets across every client (§8.2).
  unsubscribe = inbox.subscribe((event) => {
    detail.ingest(event);
    // Both before the toast: a `preference` event that arrives with an arrival
    // has to be applied first, or the toast it should have silenced is already up.
    preferences.ingest(event);
    toasts.arrival(event);
    // The badge is the server's count, so an event only says *ask again* — and
    // says it for every kind, because a revision adds to it, a tombstone takes
    // away, and a receipt of ours clears it. `refresh` coalesces the burst.
    unread.refresh();
  });

  // A push the worker handed over because this tab is visible (see `public/sw.js`)
  // — it skipped the OS notification so that this one goes through the same
  // do-not-disturb gate as the stream does.
  const onMessage = (event: MessageEvent) => {
    const message = event.data as { type?: string; envelope?: unknown } | null;
    if (message?.type === "push" && message.envelope) {
      toasts.pushed(message.envelope as { title?: string });
    }
  };
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.addEventListener("message", onMessage);
    unhandPush = () =>
      navigator.serviceWorker.removeEventListener("message", onMessage);
  }

  // Registration is not awaited by anything that renders: the app works without a
  // worker, and the person waiting for the inbox should not wait for one.
  void register(() => void resubscribe());

  // The desktop shell hands over `weebo-notify://n/<id>` clicks (§10bis.3quater).
  // `router.push` and not the History API: the guard that survives a permalink
  // opened while signed out lives on the router (see `router/index.ts`), and the
  // first draft of the shell bypassed it by driving `pushState` directly.
  // A no-op in a browser, and not awaited — the inbox must not wait for it.
  void onDeepLink((path) => void router.push(path)).then((stop) => {
    unlisten = stop;
  });

  await Promise.all([
    channels.load(),
    inbox.load(),
    preferences.load(),
    unread.load(),
  ]);
  inbox.start();
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeydown);
  unsubscribe?.();
  unhandPush?.();
  unlisten?.();
  inbox.stop();
  unread.stop();
});

/** `/n/<id>` is a permalink: opening it must open the panel (§10.4). */
watch(
  () => route.params.id,
  (id) => {
    if (route.name === "notification" && typeof id === "string") {
      void detail.open(id);
    } else if (route.name !== "notification") {
      detail.close();
    }
  },
  { immediate: true },
);

/** `/c/<id>` is the channel permalink: it narrows the inbox to that channel. */
watch(
  () => [route.name, route.params.id] as const,
  ([name, id]) => {
    if (name === "channel" && typeof id === "string") {
      void inbox.apply({ ...inbox.filters, channels: [id] });
    }
  },
  { immediate: true },
);

function closeDetail(): void {
  void router.push({ name: "inbox" });
}
</script>

<template>
  <CommandPalette />

  <div class="grid h-dvh grid-rows-[auto_minmax(0,1fr)] lg:grid-rows-1">
    <!-- Narrow viewports get a bar; wide ones do not need one, the sidebar is
         always there. -->
    <header
      class="flex items-center gap-2 border-b border-border px-3 py-2 lg:hidden"
    >
      <Button variant="ghost" size="icon" @click="sidebarOpen = !sidebarOpen">
        <Menu />
        <span class="sr-only">{{ t("sidebar.aria") }}</span>
      </Button>
      <span class="text-sm font-semibold">{{ t("app.name") }}</span>
    </header>

    <div class="grid min-h-0 lg:grid-cols-[16rem_minmax(0,1fr)]">
      <div
        :class="[
          'min-h-0 border-r border-border bg-card/40',
          sidebarOpen
            ? 'absolute inset-y-0 left-0 z-40 w-64 bg-card shadow-lg lg:static lg:shadow-none'
            : 'hidden lg:block',
        ]"
      >
        <AppSidebar @click="sidebarOpen = false" />
      </div>

      <!-- The list and the detail panel, always in this order in the DOM —
           `contentGridClass` and the two order classes below are what decide
           whether that reads left-to-right, right-to-left, or top-to-bottom
           (Réglages › Apparence). -->
      <div class="grid min-h-0" :class="contentGridClass">
        <main class="min-h-0 overflow-hidden" :class="mainOrderClass">
          <RouterView />
        </main>

        <!-- Only the three routes that render InboxView have a detail panel at
             all (see `showDetailPane`) — `settings`/`admin`/`console` render
             nothing here rather than an always-empty pane with nothing to show. -->
        <template v-if="showDetailPane">
          <!-- Under xl the panel is a full-screen layer, which is the mobile
               shape §10.4 describes; above it, it is the second pane, wherever
               the preference puts it. -->
          <div
            v-if="detail.id"
            class="fixed inset-0 z-30 min-h-0 border-l border-border bg-background xl:static xl:z-auto"
            :class="detailPositionClass"
          >
            <Button
              variant="ghost"
              size="icon"
              class="absolute right-2 top-2 z-10 xl:hidden"
              @click="closeDetail"
            >
              <X />
              <span class="sr-only">{{ t("layout.closeDetail") }}</span>
            </Button>
            <NotificationDetail />
          </div>
          <div
            v-else
            class="hidden min-h-0 border-l border-border xl:block"
            :class="detailPositionClass"
          >
            <NotificationDetail />
          </div>
        </template>
      </div>
    </div>
  </div>

  <!-- Outside the grid above, deliberately: it is `position: fixed` and reads
       from the viewport, not a cell — a grid child that Firefox and Chrome
       still hand an implicit row to on `lg+` (header goes `display: none`
       there, but this one stays a grid item), which is the black band this
       moved to stop being. -->
  <AppToaster />
  <PushPrompt />
</template>
