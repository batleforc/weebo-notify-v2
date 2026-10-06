/** UI language detection, dependency-free so `stores/ui.ts` and `i18n.ts` avoid a cycle. */

export type Locale = "en" | "fr";

export const LOCALE_KEY = "weebo-notify:locale";

/** `localStorage` choice → browser language → French, in that order. */
export function detectLocale(): Locale {
  const stored = localStorage.getItem(LOCALE_KEY);
  if (stored === "en" || stored === "fr") {
    return stored;
  }
  // `navigator.language` may be missing in stubbed environments.
  return navigator.language?.slice(0, 2).toLowerCase() === "en" ? "en" : "fr";
}
