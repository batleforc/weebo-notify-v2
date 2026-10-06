<script setup lang="ts">
/**
 * The command palette (design §10.4) — one surface to search and to switch.
 *
 * Built on Reka UI's dialog and listbox behaviour (ADR-0004) rather than on a
 * keyboard handler written here: focus trapping, restoring focus to whatever was
 * focused before, and `Esc` are the parts a hand-rolled modal gets wrong, and §10.4
 * asks for all three ("`Esc` ferme et rend le focus à l'élément d'origine — l'app
 * doit être pilotable sans souris").
 *
 * What it offers and in what order is decided by `lib/palette.ts`, and what it asks
 * the server is decided by `stores/palette.ts`. Both were tested from the day this
 * was written; how it *looked* was not, because there was no browser in the
 * workspace (§14.4, D21). There is one now (§12.4), and opening this on a real
 * list of fifteen channels showed three things a jsdom suite cannot ask about:
 *
 *   * the keyboard cursor and the mouse hover rendered the *same* grey, so two
 *     rows looked equally chosen and `Enter` was a guess. The active row now
 *     carries a left bar and a weight as well as a background — the same "never
 *     colour alone" rule §10.3 applies to statuses, for the same reason;
 *   * wrapping from the first row to the last selected something **off-screen**,
 *     because nothing scrolled the list. A palette that cannot be driven to its
 *     own last row is not the mouse-free surface §10.4 asks for;
 *   * a screen reader was told nothing at all: a `<div>` of `<button>`s has no
 *     selection to announce. It is a listbox now, and says which row is current.
 */
import {
  DialogContent,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
} from "reka-ui";
import { computed, nextTick, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import type { PaletteItem } from "@/lib/palette";
import { usePalette } from "@/stores/palette";
import { usePreferences } from "@/stores/preferences";

const palette = usePalette();
const preferences = usePreferences();
const router = useRouter();
const { t } = useI18n();

const input = ref<HTMLInputElement | null>(null);
const active = ref(0);

/** The rows, in the groups §10.4 names, empty groups dropped. */
const groups = computed(() => {
  const titles: Record<PaletteItem["group"], string> = {
    channels: t("commandPalette.groups.channels"),
    notifications: t("commandPalette.groups.notifications"),
    screens: t("commandPalette.groups.screens"),
    actions: t("commandPalette.groups.actions"),
  };
  const order: Array<PaletteItem["group"]> = [
    "channels",
    "notifications",
    "screens",
    "actions",
  ];

  // Each row carries the index it has in the flat walk below. Computed here
  // rather than searched for at render time, so that the position the arrows move
  // through and the position a row reports are the same number by construction.
  let index = 0;
  return order
    .map((group) => ({
      group,
      title: titles[group],
      items: palette.items
        .filter((item) => item.group === group)
        .map((item) => ({ item, index: index++ })),
    }))
    .filter((entry) => entry.items.length > 0);
});

/** The flat order the arrows walk, which is the order the groups render in. */
const walk = computed(() =>
  groups.value.flatMap((entry) => entry.items.map((row) => row.item)),
);

watch(
  () => palette.items,
  () => {
    // A selection that survives a change of list points at whatever moved into
    // that position, which is how somebody opens the wrong thing.
    active.value = 0;
  },
);

watch(
  () => palette.open,
  async (open) => {
    if (open) {
      active.value = 0;
      await nextTick();
      input.value?.focus();
    }
  },
);

/** The DOM id of a row, which is what `aria-activedescendant` has to name. */
function rowId(index: number): string {
  return `palette-option-${index}`;
}

function move(delta: number): void {
  const count = walk.value.length;
  if (count === 0) {
    return;
  }
  // Wrapping, because a list that stops at both ends makes people reach for the
  // mouse to get back to the top.
  active.value = (active.value + delta + count) % count;
  void reveal();
}

/**
 * Bring the active row into view.
 *
 * The list scrolls (`max-h-[50vh]`) and the selection is ours rather than the
 * browser's — no element is focused, so nothing scrolls on its own. Wrapping from
 * the first row to the last is where that bites hardest: the selection lands at
 * the bottom of a list still showing its top.
 *
 * `block: "nearest"` so a row already visible does not make the list jump.
 * Guarded, because jsdom has no layout and therefore no `scrollIntoView`.
 */
async function reveal(): Promise<void> {
  await nextTick();
  const row = document.getElementById(rowId(active.value));
  row?.scrollIntoView?.({ block: "nearest" });
}

async function choose(item: PaletteItem | undefined): Promise<void> {
  if (!item) {
    return;
  }

  palette.hide();

  if (item.action.kind === "navigate") {
    await router.push(item.action.to);
    return;
  }

  // The verbs are the TUI's (§9.2). Only the ones with a meaning outside a
  // selection are wired here; the rest arrive with the selection they act on.
  if (item.action.verb === "dnd:1h") {
    await preferences.quietFor(60);
  }
}
</script>

<template>
  <DialogRoot :open="palette.open" @update:open="(value) => !value && palette.hide()">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-ground/80" />
      <DialogContent
        class="fixed left-1/2 top-[calc(env(safe-area-inset-top)+0.75rem)] z-50 w-[min(40rem,calc(100vw-1.5rem))] -translate-x-1/2 overflow-hidden border border-rule-strong bg-popover text-popover-foreground sm:top-[15vh]"
        :aria-label="t('commandPalette.ariaLabel')"
        @keydown.down.prevent="move(1)"
        @keydown.up.prevent="move(-1)"
        @keydown.enter.prevent="choose(walk[active])"
      >
        <input
          ref="input"
          :value="palette.query"
          type="text"
          class="w-full border-b border-rule-strong bg-ground-sunk px-4 py-3.5 text-[0.9375rem] outline-none placeholder:text-ink-dim"
          :placeholder="t('commandPalette.searchPlaceholder')"
          role="combobox"
          aria-controls="palette-results"
          aria-expanded="true"
          autocomplete="off"
          :aria-activedescendant="walk.length ? rowId(active) : undefined"
          @input="palette.type(($event.target as HTMLInputElement).value)"
        />

        <div
          id="palette-results"
          role="listbox"
          :aria-label="t('commandPalette.resultsAriaLabel')"
          class="max-h-[50vh] overflow-y-auto p-1"
        >
          <p
            v-if="palette.error"
            class="px-3 py-2 text-sm text-signal"
            role="alert"
          >
            {{ palette.error }}
          </p>

          <div v-for="entry in groups" :key="entry.group" class="py-1">
            <p class="meta px-3 py-1">
              {{ entry.title }}
            </p>
            <button
              v-for="row in entry.items"
              :id="rowId(row.index)"
              :key="row.item.id"
              type="button"
              role="option"
              :aria-selected="row.index === active"
              class="flex min-h-10 w-full items-center gap-2 border-l px-3 py-2 text-left text-sm"
              :class="
                row.index === active
                  ? 'border-signal bg-ground-raised font-bold text-ink'
                  : 'border-transparent text-ink-dim hover:bg-ground-raised hover:text-ink'
              "
              @click="choose(row.item)"
            >
              <span class="truncate">{{ row.item.label }}</span>
              <span
                v-if="row.item.hint && row.item.hint !== row.item.label"
                class="ml-auto truncate text-xs text-ink-dim"
              >
                {{ row.item.hint }}
              </span>
            </button>
          </div>

          <p
            v-if="walk.length === 0 && !palette.searching"
            class="px-3 py-6 text-center text-sm text-ink-dim"
          >
            {{ t("commandPalette.empty") }}
          </p>
          <p
            v-else-if="palette.searching"
            class="px-3 py-2 text-xs text-ink-dim"
          >
            {{ t("commandPalette.searching") }}
          </p>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>
