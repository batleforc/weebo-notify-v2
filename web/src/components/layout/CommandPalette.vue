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
 * the server is decided by `stores/palette.ts`. Both are tested; the appearance of
 * this file is not, and cannot be here — there is no browser in the development
 * environment (§14.4, D21).
 */
import {
  DialogContent,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
} from "reka-ui";
import { computed, nextTick, ref, watch } from "vue";
import { useRouter } from "vue-router";

import type { PaletteItem } from "@/lib/palette";
import { usePalette } from "@/stores/palette";
import { usePreferences } from "@/stores/preferences";

const palette = usePalette();
const preferences = usePreferences();
const router = useRouter();

const input = ref<HTMLInputElement | null>(null);
const active = ref(0);

/** The rows, in the groups §10.4 names, empty groups dropped. */
const groups = computed(() => {
  const titles: Record<PaletteItem["group"], string> = {
    channels: "Canaux",
    notifications: "Notifications",
    screens: "Écrans",
    actions: "Actions",
  };
  const order: Array<PaletteItem["group"]> = [
    "channels",
    "notifications",
    "screens",
    "actions",
  ];

  return order
    .map((group) => ({
      group,
      title: titles[group],
      items: palette.items.filter((item) => item.group === group),
    }))
    .filter((entry) => entry.items.length > 0);
});

/** The flat order the arrows walk, which is the order the groups render in. */
const walk = computed(() => groups.value.flatMap((entry) => entry.items));

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

function move(delta: number): void {
  const count = walk.value.length;
  if (count === 0) {
    return;
  }
  // Wrapping, because a list that stops at both ends makes people reach for the
  // mouse to get back to the top.
  active.value = (active.value + delta + count) % count;
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
      <DialogOverlay class="fixed inset-0 z-50 bg-black/50" />
      <DialogContent
        class="fixed left-1/2 top-[15vh] z-50 w-[min(40rem,92vw)] -translate-x-1/2 overflow-hidden rounded-lg border border-border bg-popover text-popover-foreground shadow-lg"
        aria-label="Rechercher et naviguer"
        @keydown.down.prevent="move(1)"
        @keydown.up.prevent="move(-1)"
        @keydown.enter.prevent="choose(walk[active])"
      >
        <input
          ref="input"
          :value="palette.query"
          type="text"
          class="w-full border-b border-border bg-transparent px-4 py-3 text-sm outline-none placeholder:text-muted-foreground"
          placeholder="Chercher une notification, un canal, un écran…"
          @input="palette.type(($event.target as HTMLInputElement).value)"
        />

        <div class="max-h-[50vh] overflow-y-auto p-1">
          <p
            v-if="palette.error"
            class="px-3 py-2 text-sm text-destructive"
            role="alert"
          >
            {{ palette.error }}
          </p>

          <div v-for="entry in groups" :key="entry.group" class="py-1">
            <p class="px-3 py-1 text-xs uppercase tracking-wide text-muted-foreground">
              {{ entry.title }}
            </p>
            <button
              v-for="item in entry.items"
              :key="item.id"
              type="button"
              class="flex w-full items-center gap-2 rounded-md px-3 py-2 text-left text-sm"
              :class="
                walk[active]?.id === item.id
                  ? 'bg-accent text-accent-foreground'
                  : 'hover:bg-accent/50'
              "
              @click="choose(item)"
            >
              <span class="truncate">{{ item.label }}</span>
              <span
                v-if="item.hint && item.hint !== item.label"
                class="ml-auto truncate text-xs text-muted-foreground"
              >
                {{ item.hint }}
              </span>
            </button>
          </div>

          <p
            v-if="walk.length === 0 && !palette.searching"
            class="px-3 py-6 text-center text-sm text-muted-foreground"
          >
            Rien ne correspond.
          </p>
          <p
            v-else-if="palette.searching"
            class="px-3 py-2 text-xs text-muted-foreground"
          >
            Recherche…
          </p>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>
