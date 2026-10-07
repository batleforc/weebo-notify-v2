/**
 * The shared `vue-i18n` instance, for app chrome only (notification content stays
 * verbatim). Exported for plain modules that can't call `useI18n()`.
 */
import { createI18n } from "vue-i18n";
import { detectLocale, type Locale } from "@/lib/locale";
import en from "@/locales/en.json";
import fr from "@/locales/fr.json";

export type MessageSchema = typeof en;

export const i18n = createI18n<MessageSchema, Locale, false>({
  legacy: false,
  locale: detectLocale(),
  // French is canonical; missing English keys fall back to it.
  fallbackLocale: "fr",
  messages: { en, fr },
});
