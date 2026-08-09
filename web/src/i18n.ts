/**
 * The one `vue-i18n` instance the app shares (design: app chrome only, never
 * user-generated notification content — titles/bodies/tags stay verbatim).
 *
 * Exported rather than only installed in `main.ts`: a handful of plain `.ts`
 * modules (`api/problem.ts`, `lib/notification.ts`) have no component instance
 * to call `useI18n()` from, so they reach for `i18n.global.t(...)` directly.
 */
import { createI18n } from "vue-i18n";
import { detectLocale, type Locale } from "@/lib/locale";
import en from "@/locales/en.json";
import fr from "@/locales/fr.json";

export type MessageSchema = typeof en;

export const i18n = createI18n<MessageSchema, Locale, false>({
  legacy: false,
  locale: detectLocale(),
  // French is the canonical source: every key in `fr.json` exists first, `en.
  // json` is a translation of it. `fallbackLocale` stays `fr` so a key missed
  // in `en.json` degrades to real copy instead of a raw key string.
  fallbackLocale: "fr",
  messages: { en, fr },
});
