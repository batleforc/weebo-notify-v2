<script setup lang="ts">
/**
 * The three-zone shell (§10.4): channels, inbox, detail.
 *
 * The same component serves the browser, and later the desktop and mobile shells
 * (§10bis) — what changes is the breakpoint, not the code. Under `lg` the sidebar
 * folds away behind a button and the detail takes the whole screen when something
 * is open, which is the mobile behaviour §10.4 asks for.
 */
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

/**
 * Whether this route has a detail panel at all. Only the three that render
 * `InboxView` do (§10.4) — `settings`/`admin`/`console` are full pages of their
 * own, and reserving 22rem for an always-empty panel on them is not a smaller
 * version of the three-zone shell, it is 22rem taken from a page that never had
 * anything to put there. `under` is what makes this worth naming: on those
 * routes it used to leave `<main>` visibly short of the viewport, by exactly the
 * height the empty panel still claimed off-screen.
 */
/** The routes that show the inbox, and therefore care about the channel. */
const INBOX_ROUTES = [
  "inbox",
  "notification",
  "channel",
  "channel-notification",
];

/** The routes that open the detail panel. */
const DETAIL_ROUTES = ["notification", "channel-notification"];

/**
 * The channel the current route is in, if any.
 *
 * One expression for both channel routes: `/c/<id>` and `/c/<id>/n/<n>` carry it
 * in the same parameter, which is the point of nesting them — reading a
 * notification does not leave the channel it was read from.
 */
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

const showDetailPane = computed(() =>
  INBOX_ROUTES.includes(String(route.name ?? "")),
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
/**
 * Whether the detail panel is a *pane under the list* rather than a layer over it.
 *
 * Side-by-side needs width, which is why `left` and `right` stay gated on `xl` and
 * fall back to the full-screen layer below it. Stacking needs none: two rows fit a
 * phone exactly as well as a monitor. Gating it on `xl` anyway is what made a
 * window narrower than 1280px answer "detail under the list" with a layer covering
 * the list — the one arrangement where the person asked to see both at once was
 * the one where they saw one.
 */
const detailStacked = computed(
  () => showDetailPane.value && ui.detailPosition === "under",
);

const contentGridClass = computed(() => {
  if (!showDetailPane.value) {
    return "";
  }
  switch (ui.detailPosition) {
    case "left":
      return "xl:grid-cols-[26rem_minmax(0,1fr)]";
    case "under":
      // The second track only exists when something is in it. Unprefixed at every
      // width otherwise, an inbox with nothing selected would reserve 22rem of
      // blank below the list on a small window — the placeholder that fills it
      // above `xl` is `hidden` there.
      return detail.id
        ? "grid-rows-[minmax(0,1fr)_22rem]"
        : "xl:grid-rows-[minmax(0,1fr)_22rem]";
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
      // Kept, and not folded into the `detailStacked` branch below: this class also
      // dresses the *empty* placeholder, which is a plain `border-l` element that
      // has always relied on these `xl:` overrides to move its divider to the top
      // edge. Dropping them left the placeholder drawing a left border under the
      // list, where there is nothing to its left.
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

  // The stream first, then the page: a stream opened after the load starts at
  // "now" and misses whatever was published while the page was on its way
  // (`inbox.start`).
  inbox.start();
  await Promise.all([
    channels.load(),
    inbox.load(),
    preferences.load(),
    unread.load(),
  ]);
});

onBeforeUnmount(() => {
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

/** `/n/<id>` — and `/c/<channel>/n/<id>` — are permalinks: opening one must open
 * the panel (§10.4). */
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

/**
 * The channel is the route, and the route is the channel.
 *
 * `/c/<id>` narrows the inbox to it and `/` widens back to everything — both
 * directions, which is what was missing: only the first was handled, so leaving a
 * channel left its filter behind and the inbox stayed narrowed while the URL said
 * otherwise. The sidebar now navigates rather than toggling a filter, so this
 * watcher is the single place the two are reconciled.
 *
 * Routes that are not the inbox are left alone on purpose. Settings, Admin and
 * Console draw the same sidebar, and clearing the filter on the way there would
 * reload a list nobody is looking at and lose the channel on the way back.
 */
watch(
  () => [route.name, routeChannel.value] as const,
  ([name, channel]) => {
    if (!INBOX_ROUTES.includes(String(name ?? ""))) {
      return;
    }
    const next = channel === null ? [] : [channel];
    const current = inbox.filters.channels;
    // Compared rather than applied unconditionally: `apply` reloads the page and
    // restarts the stream, and every `/n/<id>` opened from within a channel would
    // otherwise do both for no change at all.
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

/**
 * Close the panel by leaving the notification's route — back to the channel it was
 * read from when there is one, rather than out to every channel. Closing a message
 * is not leaving the list you found it in.
 */
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

    <!--
      The way out of the drawer, and the reason it is an element rather than a
      document-level click handler: a listener on `document` has to decide whether
      each click landed inside the panel, which means knowing the panel's bounds and
      getting it wrong for anything that teleports out of it (a native menu, a
      tooltip). A scrim answers by construction — the only clicks it receives are
      the ones that missed.

      `z-30`, one below the drawer's `z-40`, so it covers the content and not the
      panel. It also covers the header, which puts it over the burger button: a
      click there closes rather than toggles, which is the same outcome by a
      shorter path.

      `lg:hidden` because above `lg` the sidebar is `lg:static` — part of the grid,
      covering nothing, with nothing to dismiss.
    -->
    <div
      v-if="sidebarOpen"
      class="fixed inset-0 z-30 bg-ground/80 lg:hidden"
      aria-hidden="true"
      @click="sidebarOpen = false"
    />

    <div class="grid min-h-0 lg:grid-cols-[17rem_minmax(0,1fr)]">
      <!--
        `cn` and not a plain array, and that is the whole fix rather than a tidy-up.
        Both `bg-card/40` and `bg-card` used to land on this element at once, and an
        array decides nothing: the winner is whichever rule Tailwind emitted last,
        which is `.bg-card\/40` (it carries an opacity modifier, so it sorts after
        the bare utility). The open drawer therefore rendered at 40% opacity with
        the inbox showing straight through it — which reads as a stacking bug and is
        not one. `cn` runs tailwind-merge, so the later argument wins for real.
      -->
      <div
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
            <!--
              `xl:hidden` and nothing more. This briefly also hid on `detailStacked`,
              on the reasoning that a pane covers nothing and `NotificationDetail`
              carries its own close — and that second half is false: that button
              lives in the `detail.gone` branch, so a *loaded* notification has only
              the copy-permalink one. Removing this left `Escape` as the sole way to
              dismiss the pane, which on a touch device is no way at all.
            -->
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
      <button type="button" :class="tabClass(sidebarOpen)" :aria-expanded="sidebarOpen" @click="sidebarOpen = !sidebarOpen">
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

  <!-- Outside the grid above, deliberately: it is `position: fixed` and reads
       from the viewport, not a cell — a grid child that Firefox and Chrome
       still hand an implicit row to on `lg+` (header goes `display: none`
       there, but this one stays a grid item), which is the black band this
       moved to stop being. -->
  <AppToaster />
  <PushPrompt />
</template>
