/**
 * Theme, density, layout and locale (§10.2), persisted in `localStorage`. The
 * inline script in `index.html` applies theme/density before first paint.
 */

import { defineStore } from "pinia";
import { ref, watch } from "vue";
import { i18n } from "@/i18n";
import { detectLocale, LOCALE_KEY, type Locale } from "@/lib/locale";

export type Theme = "dark" | "light" | "system";
export type Density = "compact" | "comfortable";
export type { Locale };
/** Detail panel position above `xl` (§10.4); below it the panel is a full-screen layer. */
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
  // Collapsed on first visit: `null !== "false"`.
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

  // AppLayout reads this directly; no DOM class.
  watch(detailPosition, (value) => {
    localStorage.setItem(DETAIL_POSITION_KEY, value);
  });

  watch(filtersCollapsed, (value) => {
    localStorage.setItem(FILTERS_COLLAPSED_KEY, String(value));
  });

  // `immediate`: unlike theme/density, nothing else sets the initial locale.
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

  /** `dark` → `light` → `system`: an explicit toggle offers fixed choices first. */
  function cycleTheme(): void {
    theme.value =
      theme.value === "dark"
        ? "light"
        : theme.value === "light"
          ? "system"
          : "dark";
  }

  /** `right` → `left` → `under`. */
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
