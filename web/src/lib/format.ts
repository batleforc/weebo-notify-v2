/**
 * Dates and durations, without pulling in a date library.
 *
 * `Intl.RelativeTimeFormat` ships in every browser this app targets, and an inbox
 * needs exactly two things from it: "3 min ago" on a row and a full timestamp
 * in the tooltip next to it. Both formatters are cached per UI locale rather
 * than built once at import time, since `ui.locale` can change at runtime.
 */

import { type Locale, useUi } from "@/stores/ui";

const RELATIVE_LOCALE: Record<Locale, string> = { fr: "fr", en: "en" };
const ABSOLUTE_LOCALE: Record<Locale, string> = { fr: "fr-FR", en: "en-US" };
/** The `delay()` suffixes — a fixed abbreviation, not a plural, so a lookup
 * table rather than a `t()` call carries this. */
const DELAY_UNITS: Record<Locale, { d: string; h: string; min: string }> = {
  fr: { d: "j", h: "h", min: "min" },
  en: { d: "d", h: "h", min: "min" },
};

const relativeCache = new Map<Locale, Intl.RelativeTimeFormat>();
function relativeFormatter(locale: Locale): Intl.RelativeTimeFormat {
  let formatter = relativeCache.get(locale);
  if (!formatter) {
    formatter = new Intl.RelativeTimeFormat(RELATIVE_LOCALE[locale], {
      numeric: "auto",
    });
    relativeCache.set(locale, formatter);
  }
  return formatter;
}

const absoluteCache = new Map<Locale, Intl.DateTimeFormat>();
function absoluteFormatter(locale: Locale): Intl.DateTimeFormat {
  let formatter = absoluteCache.get(locale);
  if (!formatter) {
    formatter = new Intl.DateTimeFormat(ABSOLUTE_LOCALE[locale], {
      dateStyle: "medium",
      timeStyle: "medium",
    });
    absoluteCache.set(locale, formatter);
  }
  return formatter;
}

const STEPS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["second", 60],
  ["minute", 60],
  ["hour", 24],
  ["day", 7],
  ["week", 4.348],
  ["month", 12],
  ["year", Number.POSITIVE_INFINITY],
];

/** "3 min ago", "in 2 h". Empty string for anything unparseable. */
export function ago(iso: string, now = Date.now()): string {
  const at = Date.parse(iso);
  if (Number.isNaN(at)) {
    return "";
  }

  const relative = relativeFormatter(useUi().locale);
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
  return Number.isNaN(at) ? iso : absoluteFormatter(useUi().locale).format(at);
}

/**
 * A delay as the late badge words it: `+2h13` (§7.2).
 *
 * Deliberately not `Intl.RelativeTimeFormat`: this is a magnitude, not a moment,
 * and "about 2 hours ago" hides the thirteen minutes an incident review cares
 * about.
 */
export function delay(ms: number): string {
  const units = DELAY_UNITS[useUi().locale];
  const seconds = Math.max(0, Math.round(ms / 1000));
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);

  if (days > 0) {
    return `+${days}${units.d}${String(hours).padStart(2, "0")}`;
  }
  if (hours > 0) {
    return `+${hours}${units.h}${String(minutes).padStart(2, "0")}`;
  }
  if (minutes > 0) {
    return `+${minutes}${units.min}`;
  }
  return `+${seconds}s`;
}
