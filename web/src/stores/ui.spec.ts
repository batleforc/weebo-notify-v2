/** Theme cycling and the OS preference (§10.2). */

import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";

import { useUi } from "@/stores/ui";

/** A flippable `matchMedia` stub; jsdom has none. */
function stubMatchMedia(prefersDark: boolean): void {
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: query.includes("dark") ? prefersDark : !prefersDark,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    })),
  );
}

describe("theme cycling", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.classList.remove("dark");
    stubMatchMedia(true);
    setActivePinia(createPinia());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("goes dark, then light, then system, then back to dark", () => {
    const ui = useUi();
    ui.theme = "dark";

    ui.cycleTheme();
    expect(ui.theme).toBe("light");

    ui.cycleTheme();
    expect(ui.theme).toBe("system");

    ui.cycleTheme();
    expect(ui.theme).toBe("dark");
  });

  it("system resolves to whatever the OS says, not a fixed choice", async () => {
    stubMatchMedia(false);
    const ui = useUi();
    // Watchers flush on microtasks: tick after each write, or `system → light →
    // system` looks like no change at all.
    await nextTick();
    expect(document.documentElement.classList.contains("dark")).toBe(false);

    stubMatchMedia(true);
    ui.theme = "light"; // force a change so the watcher's `applyTheme` re-reads matchMedia
    await nextTick();
    ui.theme = "system";
    await nextTick();
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("persists the choice across a reload", async () => {
    const ui = useUi();
    ui.theme = "dark"; // away from the default first, or "system" would be a no-op
    await nextTick();
    ui.theme = "system";
    await nextTick();

    expect(localStorage.getItem("weebo-notify:theme")).toBe("system");
  });
});

describe("detail panel position", () => {
  beforeEach(() => {
    localStorage.clear();
    stubMatchMedia(true);
    setActivePinia(createPinia());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("goes right, then left, then under, then back to right", () => {
    const ui = useUi();
    ui.detailPosition = "right";

    ui.cycleDetailPosition();
    expect(ui.detailPosition).toBe("left");

    ui.cycleDetailPosition();
    expect(ui.detailPosition).toBe("under");

    ui.cycleDetailPosition();
    expect(ui.detailPosition).toBe("right");
  });

  it("persists the choice across a reload", async () => {
    const ui = useUi();
    ui.detailPosition = "under";
    await nextTick();

    expect(localStorage.getItem("weebo-notify:detail-position")).toBe("under");
  });
});

describe("filtersCollapsed", () => {
  beforeEach(() => {
    localStorage.clear();
    stubMatchMedia(true);
    setActivePinia(createPinia());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("starts collapsed on a fresh localStorage", () => {
    const ui = useUi();
    expect(ui.filtersCollapsed).toBe(true);
  });

  it("stays expanded across a reload once opened", async () => {
    const ui = useUi();
    ui.filtersCollapsed = false;
    await nextTick();

    expect(localStorage.getItem("weebo-notify:filters-collapsed")).toBe(
      "false",
    );

    setActivePinia(createPinia());
    expect(useUi().filtersCollapsed).toBe(false);
  });
});
