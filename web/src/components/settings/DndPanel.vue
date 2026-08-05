<script setup lang="ts">
/**
 * Ne pas déranger (§7.4, §10.4).
 *
 * Four things, and they are four because §7.4 makes them four: a quick toggle,
 * recurring windows, how wide it reaches with its exceptions, and whether
 * `urgent` may come through.
 *
 * # The panel edits a copy and PUTs the whole thing
 *
 * `PUT /preferences/dnd` replaces rather than merges — a body that omitted a
 * window could not be told apart from one asking to drop it — so the draft here is
 * the whole preference, and "Annuler" means re-reading it. That is also what keeps
 * a change made on a phone from being clobbered: the `preference` event refreshes
 * the store, and this panel re-drafts from it.
 *
 * # The timezone is the preference's, never the device's
 *
 * §7.4 is explicit and the reason is a person travelling: `22:00–08:00,
 * Europe/Paris` means *their* nights, not the nights of wherever their laptop is
 * this month. The field is pre-filled from the browser as a convenience and stays
 * editable, and the server refuses anything that is not an IANA name.
 */
import { Plus, Trash2 } from "lucide-vue-next";
import { computed, onMounted, ref, watch } from "vue";

import type { DndDraft, QuietHours } from "@/api/generated";
import { failureOf } from "@/api/problem";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useChannels } from "@/stores/channels";
import { draftOf, usePreferences } from "@/stores/preferences";

const preferences = usePreferences();
const channels = useChannels();

const draft = ref<DndDraft>(draftOf(null));
const saving = ref(false);
const error = ref<string | null>(null);

/** The days, in the order a week is read rather than the order the enum lists. */
const DAYS = [
  { key: "mon", label: "L" },
  { key: "tue", label: "M" },
  { key: "wed", label: "M" },
  { key: "thu", label: "J" },
  { key: "fri", label: "V" },
  { key: "sat", label: "S" },
  { key: "sun", label: "D" },
] as const;

const browserTimezone = Intl.DateTimeFormat().resolvedOptions().timeZone;

/** Whether a quiet period is on *now* — the one thing a person checks first. */
const quiet = computed(() => preferences.quiet);

const scopeIsEverything = computed(
  () => (draft.value.scope?.kind ?? "everything") === "everything",
);

onMounted(async () => {
  await preferences.load();
  redraft();
});

// Somebody turning do-not-disturb on from another client should see it here
// within the round trip of the stream, not at the next reload (§7.4).
watch(() => preferences.dnd, redraft);

function redraft(): void {
  draft.value = draftOf(preferences.dnd);
}

function addWindow(): void {
  const windows: QuietHours[] = [...(draft.value.quiet_hours ?? [])];
  windows.push({
    start: "22:00",
    end: "08:00",
    timezone: browserTimezone,
    // Empty means every day, which is the common case and the one nobody should
    // have to spell out seven times.
    days: [],
  });
  draft.value = { ...draft.value, quiet_hours: windows };
}

function removeWindow(index: number): void {
  const windows = [...(draft.value.quiet_hours ?? [])];
  windows.splice(index, 1);
  draft.value = { ...draft.value, quiet_hours: windows };
}

function toggleDay(index: number, day: (typeof DAYS)[number]["key"]): void {
  const windows = [...(draft.value.quiet_hours ?? [])];
  const window = windows[index];
  const days = new Set(window.days ?? []);
  days.has(day) ? days.delete(day) : days.add(day);
  windows[index] = { ...window, days: [...days] };
  draft.value = { ...draft.value, quiet_hours: windows };
}

function toggleException(channel: string): void {
  const exceptions = new Set(draft.value.exceptions ?? []);
  exceptions.has(channel)
    ? exceptions.delete(channel)
    : exceptions.add(channel);
  draft.value = { ...draft.value, exceptions: [...exceptions] };
}

async function save(): Promise<void> {
  saving.value = true;
  error.value = null;
  try {
    await preferences.save(draft.value);
  } catch (failure) {
    // A window whose timezone does not exist, or one that starts and ends at the
    // same minute: the server names the field, so show what it said.
    error.value = failureOf(failure).message;
  } finally {
    saving.value = false;
  }
}

async function quietFor(minutes: number): Promise<void> {
  saving.value = true;
  error.value = null;
  try {
    await preferences.quietFor(minutes);
  } catch {
    error.value = "Enregistrement impossible.";
  } finally {
    saving.value = false;
  }
}

async function resume(): Promise<void> {
  saving.value = true;
  try {
    await preferences.resume();
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <Card>
    <CardHeader>
      <CardTitle class="flex items-center gap-2">
        Ne pas déranger
        <Badge v-if="quiet" variant="secondary">actif</Badge>
      </CardTitle>
    </CardHeader>
    <CardContent class="space-y-5 text-sm">
      <p class="text-muted-foreground">
        Les push OS sont supprimés pendant une période de calme ; le flux continue
        de couler et l'inbox se remplit normalement. À la sortie, un seul résumé
        plutôt qu'une rafale.
      </p>

      <!-- The quick toggle of §7.4: a duration, not a schedule. -->
      <div class="flex flex-wrap items-center gap-2">
        <Button
          v-for="option in [
            { label: '30 min', minutes: 30 },
            { label: '1 h', minutes: 60 },
            { label: '8 h', minutes: 480 },
          ]"
          :key="option.minutes"
          variant="outline"
          size="sm"
          :disabled="saving"
          @click="quietFor(option.minutes)"
        >
          Silence {{ option.label }}
        </Button>
        <Button
          v-if="draft.until"
          variant="ghost"
          size="sm"
          :disabled="saving"
          @click="resume"
        >
          Reprendre maintenant
        </Button>
      </div>
      <p v-if="draft.until" class="text-xs text-muted-foreground">
        Silence jusqu'à {{ new Date(draft.until).toLocaleString() }}.
      </p>

      <!-- Recurring windows. -->
      <div class="space-y-3">
        <div class="flex items-center justify-between">
          <span class="font-medium">Plages récurrentes</span>
          <Button variant="outline" size="sm" @click="addWindow">
            <Plus class="size-4" /> Ajouter
          </Button>
        </div>

        <p
          v-if="!draft.quiet_hours?.length"
          class="text-xs text-muted-foreground"
        >
          Aucune plage. Le silence ponctuel ci-dessus reste disponible.
        </p>

        <div
          v-for="(window, index) in draft.quiet_hours ?? []"
          :key="index"
          class="space-y-2 rounded-md border border-border p-3"
        >
          <div class="flex flex-wrap items-center gap-2">
            <Input v-model="window.start" type="time" class="w-28" />
            <span class="text-muted-foreground">→</span>
            <Input v-model="window.end" type="time" class="w-28" />
            <Input
              v-model="window.timezone"
              class="w-56"
              placeholder="Europe/Paris"
            />
            <Button
              variant="ghost"
              size="sm"
              class="ml-auto"
              aria-label="Supprimer la plage"
              @click="removeWindow(index)"
            >
              <Trash2 class="size-4" />
            </Button>
          </div>
          <div class="flex flex-wrap items-center gap-1">
            <Button
              v-for="day in DAYS"
              :key="day.key"
              size="sm"
              class="w-8"
              :variant="window.days?.includes(day.key) ? 'default' : 'outline'"
              @click="toggleDay(index, day.key)"
            >
              {{ day.label }}
            </Button>
            <span
              v-if="!window.days?.length"
              class="ml-2 text-xs text-muted-foreground"
            >
              tous les jours
            </span>
          </div>
          <p
            v-if="window.end <= window.start"
            class="text-xs text-muted-foreground"
          >
            Traverse minuit : la plage appartient au jour où elle
            <strong>commence</strong> — un vendredi 22:00–08:00 est la nuit du
            vendredi au samedi.
          </p>
        </div>
      </div>

      <!-- Scope and exceptions: the on-call case. -->
      <div class="space-y-2">
        <span class="font-medium">Canaux qui sonnent quand même</span>
        <p class="text-xs text-muted-foreground">
          Tout couper <em>sauf</em> celui qui compte — le cas astreinte.
        </p>
        <div class="flex flex-wrap gap-1">
          <Button
            v-for="channel in channels.all"
            :key="channel.id"
            size="sm"
            :variant="
              draft.exceptions?.includes(channel.id) ? 'default' : 'outline'
            "
            @click="toggleException(channel.id)"
          >
            {{ channels.nameOf(channel.id) }}
          </Button>
        </div>
        <p v-if="!scopeIsEverything" class="text-xs text-muted-foreground">
          La portée est restreinte à certains canaux ; elle se modifie depuis la
          CLI (<code>notify dnd</code>) pour l'instant.
        </p>
      </div>

      <!-- Whose call `urgent` is. -->
      <label class="flex items-start gap-2">
        <input
          v-model="draft.urgent_bypasses"
          type="checkbox"
          class="mt-1 size-4"
        />
        <span>
          Laisser passer les notifications <strong>urgent</strong>
          <span class="block text-xs text-muted-foreground">
            C'est vous qui décidez ce qui peut vous réveiller, jamais l'émetteur.
          </span>
        </span>
      </label>

      <div class="flex items-center gap-2">
        <Button :disabled="saving" @click="save">
          {{ saving ? "Enregistrement…" : "Enregistrer" }}
        </Button>
        <Button variant="ghost" :disabled="saving" @click="redraft">
          Annuler
        </Button>
      </div>

      <p v-if="error" class="text-xs text-destructive">{{ error }}</p>
    </CardContent>
  </Card>
</template>
