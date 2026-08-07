/**
 * Do-not-disturb and silences, as this client holds them (design §7.4, §8.1).
 *
 * The server is the authority: it decides what to push and it suppresses what a
 * quiet period covers, on every device at once. This store exists for the one
 * thing the server cannot do — decide whether an *open tab* should raise a toast
 * for something arriving on its own stream.
 *
 * §7.4 is explicit about that case: "les clients connectés respectent le DND
 * localement […] pendant un DND (ou un `silence` actif), ils suppriment ces popups
 * tout en continuant d'alimenter l'inbox. Sans cette règle, le SSE contournerait le
 * DND sur les clients ouverts."
 *
 * # Why the rule is written twice, and where the copy stops
 *
 * M3.1 says nothing but the domain gets to decide what "DND" means, and this file
 * is a second implementation of `Dnd::is_muted`. The alternative was asking the
 * server per arrival, which is a round trip to decide whether to draw a toast for
 * an event that already arrived — on the path where latency is the whole feature.
 *
 * So the copy is bounded on purpose: it decides **local popups only**. Nothing here
 * suppresses a push (the server did that), removes anything from the inbox, or is
 * ever asked by another module. The two are kept honest by shape rather than by
 * hope — [`muted`] mirrors `is_muted` clause for clause, in the same order, and the
 * tests use the scenarios of §7.4 that the Rust tests use.
 *
 * # It stays fresh without polling
 *
 * The `preference` event of §8.1 carries the whole state — never a delta — so a
 * client that missed one still lands on the right answer, and turning DND on from a
 * phone silences this tab within the round trip of the stream.
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

/** Minutes since local midnight, from the `HH:MM` the wire uses. */
function minutes(wallClock: string): number {
  const [hours, mins] = wallClock.split(":");
  return Number(hours) * 60 + Number(mins);
}

/** The wire's day names, in the order `Date.getDay()` counts them from Sunday. */
const DAYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"] as const;

/**
 * What the clocks read in a given IANA zone at a given instant.
 *
 * `Intl` is the browser's copy of the same IANA database `chrono-tz` compiles into
 * the server, which is what lets the two agree across a DST boundary without this
 * file knowing anything about one. An unknown zone throws — refused at the door by
 * the API (§7.4), so reaching here means a preference written before a browser
 * update, and the honest answer is "not quiet" rather than a crash in a toast.
 */
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

/**
 * Whether one recurring window covers this instant.
 *
 * Mirrors `QuietHours::covers`, including the part that is easy to get wrong: a
 * window crossing midnight belongs to the day it **starts** on, so the small hours
 * of Saturday are covered by *Friday's* entry — which is what a person means by
 * "Friday night".
 */
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
  const on = (index: number) =>
    days.length === 0 || days.includes(DAYS[(index + 7) % 7]);

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
      error.value = "Impossible de lire les préférences.";
    } finally {
      loading.value = false;
    }
  }

  /**
   * Replace the whole preference.
   *
   * `PUT` replaces rather than merges, and the API says why: a body that omits a
   * window cannot be told apart from one asking to drop it. So the caller sends
   * the whole thing, every time — which is also what makes the panel's "cancel"
   * mean something.
   */
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

  /**
   * Fold a `preference` event (§8.1).
   *
   * Whole state in, whole state out — no merging, deliberately: the event carries
   * everything precisely so a client that missed one does not compound the miss.
   */
  function ingest(event: StreamEvent): void {
    if (event.event !== "preference") {
      return;
    }
    dnd.value = event.dnd;
    silences.value = event.silences;
  }

  /**
   * Whether a local popup should be suppressed for this arrival.
   *
   * Both halves of "stop notifying me" are asked, which is why §8.1 ships them in
   * one event: a silence is reactive and targeted, a do-not-disturb is planned and
   * wide, and a client holding one without the other has a window where it buzzes
   * for something somebody just muted.
   */
  function stayQuiet(arrival: {
    notification: string;
    groupKey?: string | null;
    /**
     * What the **server** said about this delivery: `muted_by_dnd` on the event
     * (§7.4, §8.1).
     *
     * Passed in rather than recomputed here, which is D22: the do-not-disturb rule
     * has four clauses, one of them a quiet window whose small hours belong to the
     * previous day, and it used to exist three times — in the domain for push, in
     * the CLI for its desktop relay, and here. The server holds the preference and
     * applies it per reader; this client reads the answer.
     *
     * Undefined for anything that did not come from an event — a push handed over
     * by the worker, say — and undefined means "nobody said", which is not muted.
     */
    mutedByDnd?: boolean;
  }): boolean {
    // Mirrors `Silence::covers`: a group silence matches by `group_key`, and a
    // notification silence by id. A row with no group key matches neither, which
    // is why the comparison is against a value rather than against `undefined`.
    const silenced = silences.value.some((silence) =>
      silence.target.scope === "group"
        ? arrival.groupKey != null && silence.target.key === arrival.groupKey
        : silence.target.id === arrival.notification,
    );

    // Silence stays local, and that is not an oversight: it is per notification
    // and per group (§2.3), the client already holds every one of them from the
    // `preference` event, and no server round trip is involved in answering it.
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
