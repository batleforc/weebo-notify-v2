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

export type Theme = "dark" | "light" | "system";
export type Density = "compact" | "comfortable";

const THEME_KEY = "weebo-notify:theme";
const DENSITY_KEY = "weebo-notify:density";

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

  applyTheme();
  applyDensity();

  return { cycleTheme, density, theme };
});
