/**
 * UI chrome language detection — no dependency on Pinia or vue-i18n, so both
 * `stores/ui.ts` (the persisted preference) and `i18n.ts` (the initial locale
 * `createI18n` needs before any store exists) can import this without a cycle.
 */

export type Locale = "en" | "fr";

export const LOCALE_KEY = "weebo-notify:locale";

/** `localStorage` choice → browser language → French, in that order. */
export function detectLocale(): Locale {
  const stored = localStorage.getItem(LOCALE_KEY);
  if (stored === "en" || stored === "fr") {
    return stored;
  }
  // `navigator.language` is absent in some test/embedding environments (a
  // stubbed `navigator` that mocks only what its test needs, for instance) —
  // that is not a reason to throw before a locale is even decided.
  return navigator.language?.slice(0, 2).toLowerCase() === "en" ? "en" : "fr";
}
