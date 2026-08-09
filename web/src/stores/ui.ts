/**
 * Theme and density (design §10.2).
 *
 * Both are a class on `<html>`, and both are applied before the first paint by the
 * inline script in `index.html` — this store owns the choice, not the initial
 * render. Persisted in `localStorage`; §10.2 says a per-device sync comes later,
 * and later means once devices are a first-class object (M2).
 */

import { defineStore } from "pinia";
import { ref, watch } from "vue";
import { i18n } from "@/i18n";
import { detectLocale, LOCALE_KEY, type Locale } from "@/lib/locale";

export type Theme = "dark" | "light" | "system";
export type Density = "compact" | "comfortable";
export type { Locale };
/** Where the detail panel sits relative to the inbox list (§10.4), above `xl` —
 * below it the panel is always a full-screen layer, so the preference has
 * nothing to say. `right` is the default: reading left to right, the list comes
 * first and the detail is what opening a row reveals next. */
export type DetailPosition = "right" | "left" | "under";

const THEME_KEY = "weebo-notify:theme";
const DENSITY_KEY = "weebo-notify:density";
const DETAIL_POSITION_KEY = "weebo-notify:detail-position";
const FILTERS_COLLAPSED_KEY = "weebo-notify:filters-collapsed";

function systemPrefersDark(): boolean {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

export const useUi = defineStore("ui", () => {
  const theme = ref<Theme>(
    (localStorage.getItem(THEME_KEY) as Theme | null) ?? "system",
  );
  const density = ref<Density>(
    (localStorage.getItem(DENSITY_KEY) as Density | null) ?? "compact",
  );
  const detailPosition = ref<DetailPosition>(
    (localStorage.getItem(DETAIL_POSITION_KEY) as DetailPosition | null) ??
      "right",
  );
  // Collapsed on first visit: a fresh `localStorage` reads back `null`, and
  // `null !== "false"` is `true` — the sidebar's filters section starts closed
  // until somebody opens it once, which is the only choice that persists from
  // then on.
  const filtersCollapsed = ref<boolean>(
    localStorage.getItem(FILTERS_COLLAPSED_KEY) !== "false",
  );
  const locale = ref<Locale>(detectLocale());

  function applyTheme(): void {
    // Dark is the default (§10.2): `system` resolves to dark unless the system
    // says otherwise, which is also what the pre-paint script assumes.
    const dark =
      theme.value === "dark" ||
      (theme.value === "system" && systemPrefersDark());
    document.documentElement.classList.toggle("dark", dark);
  }

  function applyDensity(): void {
    document.documentElement.classList.toggle(
      "density-comfortable",
      density.value === "comfortable",
    );
  }

  watch(theme, (value) => {
    localStorage.setItem(THEME_KEY, value);
    applyTheme();
  });

  watch(density, (value) => {
    localStorage.setItem(DENSITY_KEY, value);
    applyDensity();
  });

  // No DOM class to toggle here, unlike theme/density: `AppLayout.vue` reads
  // this ref directly to compute its grid classes, so persisting it is all this
  // store owns.
  watch(detailPosition, (value) => {
    localStorage.setItem(DETAIL_POSITION_KEY, value);
  });

  watch(filtersCollapsed, (value) => {
    localStorage.setItem(FILTERS_COLLAPSED_KEY, String(value));
  });

  // `immediate: true`, unlike theme/density above: those rely on the explicit
  // `applyTheme()`/`applyDensity()` calls below to set the initial DOM state,
  // but there is no such call for locale — this watch is the only place
  // `<html lang>`/the i18n composer's locale ever get set, so it has to run
  // once for the initial value too, not just on change.
  watch(
    locale,
    (value) => {
      localStorage.setItem(LOCALE_KEY, value);
      i18n.global.locale.value = value;
      document.documentElement.lang = value;
    },
    { immediate: true },
  );

  // Somebody switching their desktop to light at sunset expects the app to
  // follow, but only while they have not made a choice of their own.
  window
    .matchMedia("(prefers-color-scheme: dark)")
    .addEventListener("change", () => {
      if (theme.value === "system") {
        applyTheme();
      }
    });

  /**
   * `dark` → `light` → `system` → `dark`. `system` last, not first: someone
   * reaching for this toggle is making an explicit choice, and cycling past the
   * one option that is not a fixed choice — it re-delegates to the OS — before
   * looping back reads as "give up and follow the system" rather than "here is
   * a third color".
   */
  function cycleTheme(): void {
    theme.value =
      theme.value === "dark"
        ? "light"
        : theme.value === "light"
          ? "system"
          : "dark";
  }

  /**
   * `right` → `left` → `under` → `right`. `right` first, because that is the
   * default a reset leaves nobody surprised by (§10.4's own three-zone shell).
   */
  function cycleDetailPosition(): void {
    detailPosition.value =
      detailPosition.value === "right"
        ? "left"
        : detailPosition.value === "left"
          ? "under"
          : "right";
  }

  applyTheme();
  applyDensity();

  return {
    cycleDetailPosition,
    cycleTheme,
    density,
    detailPosition,
    filtersCollapsed,
    locale,
    theme,
  };
});
