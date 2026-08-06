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
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import AppSidebar from "@/components/layout/AppSidebar.vue";
import AppToaster from "@/components/layout/AppToaster.vue";
import CommandPalette from "@/components/layout/CommandPalette.vue";
import NotificationDetail from "@/components/notification/NotificationDetail.vue";
import { Button } from "@/components/ui/button";
import { opensPalette } from "@/lib/palette";
import { resubscribe } from "@/lib/push";
import { register } from "@/lib/service-worker";
import { useChannels } from "@/stores/channels";
import { useDetail } from "@/stores/detail";
import { useInbox } from "@/stores/inbox";
import { usePalette } from "@/stores/palette";
import { usePreferences } from "@/stores/preferences";
import { useToasts } from "@/stores/toasts";

const channels = useChannels();
const detail = useDetail();
const inbox = useInbox();
const preferences = usePreferences();
const toasts = useToasts();
const route = useRoute();
const router = useRouter();

const sidebarOpen = ref(false);
const palette = usePalette();

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

  await Promise.all([channels.load(), inbox.load(), preferences.load()]);
  inbox.start();
});

onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeydown);
  unsubscribe?.();
  unhandPush?.();
  inbox.stop();
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
        <span class="sr-only">Canaux et filtres</span>
      </Button>
      <span class="text-sm font-semibold">weebo-notify</span>
    </header>

    <div
      class="grid min-h-0 lg:grid-cols-[16rem_minmax(0,1fr)] xl:grid-cols-[16rem_minmax(0,1fr)_26rem]"
    >
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

      <main class="min-h-0 overflow-hidden">
        <RouterView />
      </main>

      <!-- Under xl the panel is a full-screen layer, which is the mobile shape
           §10.4 describes; above it, it is the third column. -->
      <div
        v-if="detail.id"
        class="fixed inset-0 z-30 min-h-0 border-l border-border bg-background xl:static xl:z-auto"
      >
        <Button
          variant="ghost"
          size="icon"
          class="absolute right-2 top-2 z-10 xl:hidden"
          @click="closeDetail"
        >
          <X />
          <span class="sr-only">Fermer le détail</span>
        </Button>
        <NotificationDetail />
      </div>
      <div v-else class="hidden min-h-0 border-l border-border xl:block">
        <NotificationDetail />
      </div>
    </div>
    <AppToaster />
  </div>
</template>
