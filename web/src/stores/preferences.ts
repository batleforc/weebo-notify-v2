/**
 * Do-not-disturb and silences (§7.4, §8.1). The server decides pushes; this store
 * only decides whether an open tab raises a toast for a stream arrival. Kept fresh
 * by the `preference` event, which always carries the whole state.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type {
  Dnd,
  DndDraft,
  Silence,
  Event as StreamEvent,
} from "@/api/generated";
import { readDnd, setDnd } from "@/api/generated";
import { i18n } from "@/i18n";

/** Minutes since local midnight, from the `HH:MM` the wire uses. */
function minutes(wallClock: string): number {
  const [hours, mins] = wallClock.split(":");
  return Number(hours) * 60 + Number(mins);
}

/** The wire's day names, in the order `Date.getDay()` counts them from Sunday. */
const DAYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"] as const;

/** Wall clock in an IANA zone, via `Intl`. An unknown zone yields `null` ("not quiet"), never a crash. */
function localise(
  timezone: string,
  at: Date,
): { minutes: number; day: number } | null {
  try {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone: timezone,
      hourCycle: "h23",
      hour: "2-digit",
      minute: "2-digit",
      weekday: "short",
    }).formatToParts(at);

    const read = (type: string) =>
      parts.find((part) => part.type === type)?.value ?? "";

    const weekday = read("weekday").toLowerCase().slice(0, 3);
    const day = DAYS.indexOf(weekday as (typeof DAYS)[number]);
    if (day < 0) {
      return null;
    }

    return { minutes: Number(read("hour")) * 60 + Number(read("minute")), day };
  } catch {
    return null;
  }
}

/** Mirrors `QuietHours::covers`: a window crossing midnight belongs to the day it starts on. */
export function covers(
  window: { start: string; end: string; timezone: string; days?: string[] },
  at: Date,
): boolean {
  const local = localise(window.timezone, at);
  if (!local) {
    return false;
  }

  const start = minutes(window.start);
  const end = minutes(window.end);
  const days = window.days ?? [];
  const on = (index: number) => {
    const day = DAYS[(index + 7) % 7];
    return days.length === 0 || (day !== undefined && days.includes(day));
  };

  if (start < end) {
    return on(local.day) && local.minutes >= start && local.minutes < end;
  }

  return (
    (local.minutes >= start && on(local.day)) ||
    (local.minutes < end && on(local.day - 1))
  );
}

/** Mirrors `Dnd::is_active`: the quick toggle, or any window that covers now. */
export function active(dnd: Dnd | null, at: Date): boolean {
  if (!dnd) {
    return false;
  }
  if (dnd.until && at < new Date(dnd.until)) {
    return true;
  }
  return (dnd.quiet_hours ?? []).some((window) => covers(window, at));
}

export const usePreferences = defineStore("preferences", () => {
  const dnd = ref<Dnd | null>(null);
  const silences = ref<Silence[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

  /** Whether a quiet period is on right now — the switch Settings draws. */
  const quiet = computed(() => active(dnd.value, new Date()));

  async function load(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      const answer = await readDnd({ throwOnError: true });
      dnd.value = answer.data;
    } catch {
      error.value = i18n.global.t("settings.dnd.loadFailed");
    } finally {
      loading.value = false;
    }
  }

  /** `PUT` replaces, so the caller always sends the whole preference. */
  async function save(draft: DndDraft): Promise<void> {
    const answer = await setDnd({ body: draft, throwOnError: true });
    dnd.value = answer.data;
  }

  /** The quick toggle of §7.4: quiet for a duration, whatever the schedule says. */
  async function quietFor(minutesAhead: number): Promise<void> {
    const until = new Date(Date.now() + minutesAhead * 60_000);
    await save({ ...draftOf(dnd.value), until: until.toISOString() });
  }

  /** End the quick toggle. The schedule, if any, keeps applying. */
  async function resume(): Promise<void> {
    await save({ ...draftOf(dnd.value), until: null });
  }

  /** Fold a `preference` event (§8.1): whole state, never merged. */
  function ingest(event: StreamEvent): void {
    if (event.event !== "preference") {
      return;
    }
    dnd.value = event.dnd;
    silences.value = event.silences;
  }

  /** Whether a local popup should be suppressed: silenced, or muted by DND per the server. */
  function stayQuiet(arrival: {
    notification: string;
    groupKey?: string | null;
    /** The server's `muted_by_dnd` (§7.4, D22); undefined (e.g. a forwarded push) means not muted. */
    mutedByDnd?: boolean;
  }): boolean {
    // Mirrors `Silence::covers`; a row without a group key never matches a group silence.
    const silenced = silences.value.some((silence) =>
      silence.target.scope === "group"
        ? arrival.groupKey != null && silence.target.key === arrival.groupKey
        : silence.target.id === arrival.notification,
    );

    return silenced || (arrival.mutedByDnd ?? false);
  }

  return {
    dnd,
    silences,
    loading,
    error,
    quiet,
    load,
    save,
    quietFor,
    resume,
    ingest,
    stayQuiet,
  };
});

/** The editable half of a `Dnd` — everything except whose it is. */
export function draftOf(dnd: Dnd | null): DndDraft {
  return {
    quiet_hours: dnd?.quiet_hours ?? [],
    until: dnd?.until ?? null,
    scope: dnd?.scope ?? { kind: "everything" },
    exceptions: dnd?.exceptions ?? [],
    urgent_bypasses: dnd?.urgent_bypasses ?? false,
  };
}
