<script setup lang="ts">
/**
 * The command palette (§10.4) on Reka UI's dialog (ADR-0004) for focus trap/restore
 * and `Esc`. Content and ranking live in `lib/palette.ts` and `stores/palette.ts`.
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

  // Precomputed flat index, so arrow position and row id always agree.
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
    // Reset, or the selection points at whatever moved into its slot.
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
  active.value = (active.value + delta + count) % count;
  void reveal();
}

/** The selection isn't focus, so nothing scrolls by itself. jsdom lacks `scrollIntoView`. */
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

  // TUI verbs (§9.2); only selection-free ones are wired here.
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
