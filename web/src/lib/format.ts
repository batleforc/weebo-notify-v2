/**
 * Dates and durations, in French, without pulling in a date library.
 *
 * `Intl.RelativeTimeFormat` ships in every browser this app targets, and an inbox
 * needs exactly two things from it: "il y a 3 min" on a row and a full timestamp
 * in the tooltip next to it.
 */

const relative = new Intl.RelativeTimeFormat("fr", { numeric: "auto" });

const absolute = new Intl.DateTimeFormat("fr-FR", {
  dateStyle: "medium",
  timeStyle: "medium",
});

const STEPS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["second", 60],
  ["minute", 60],
  ["hour", 24],
  ["day", 7],
  ["week", 4.348],
  ["month", 12],
  ["year", Number.POSITIVE_INFINITY],
];

/** "il y a 3 min", "dans 2 h". Empty string for anything unparseable. */
export function ago(iso: string, now = Date.now()): string {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) {
    return "";
  }

  let delta = (at - now) / 1000;
  for (const [unit, span] of STEPS) {
    if (Math.abs(delta) < span) {
      return relative.format(Math.round(delta), unit);
    }
    delta /= span;
  }
  return relative.format(Math.round(delta), "year");
}

/** The full timestamp, for a tooltip — the row shows the relative one. */
export function exact(iso: string): string {
  const at = Date.parse(iso);
  return Number.isNaN(at) ? iso : absolute.format(at);
}

/**
 * A delay as the late badge words it: `+2h13` (§7.2).
 *
 * Deliberately not `Intl.RelativeTimeFormat`: this is a magnitude, not a moment,
 * and "il y a environ 2 heures" hides the thirteen minutes an incident review
 * cares about.
 */
export function delay(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);

  if (days > 0) {
    return `+${days}j${String(hours).padStart(2, "0")}`;
  }
  if (hours > 0) {
    return `+${hours}h${String(minutes).padStart(2, "0")}`;
  }
  if (minutes > 0) {
    return `+${minutes}min`;
  }
  return `+${seconds}s`;
}
