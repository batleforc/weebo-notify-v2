import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "@/stores/ui";
import { ago, delay, exact } from "./format";

beforeEach(() => {
  // `useUi()`'s setup touches `matchMedia` for the theme system-preference
  // listener — irrelevant here, but the store still has to construct.
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockReturnValue({
      matches: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    }),
  );
  setActivePinia(createPinia());
  useUi().locale = "fr";
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("delay", () => {
  it("words a lateness the way the badge does (§7.2)", () => {
    expect(delay(2 * 3_600_000 + 13 * 60_000)).toBe("+2h13");
    expect(delay(45 * 60_000)).toBe("+45min");
    expect(delay(9_000)).toBe("+9s");
    expect(delay(50 * 3_600_000)).toBe("+2j02");
  });

  it("keeps the minutes an incident review cares about", () => {
    // The whole reason this is not `Intl.RelativeTimeFormat`: "il y a environ
    // 2 heures" loses exactly the thirteen minutes somebody is looking for.
    expect(delay(2 * 3_600_000 + 13 * 60_000)).not.toContain("environ");
  });

  it("never shows a negative delay", () => {
    expect(delay(-5_000)).toBe("+0s");
  });
});

describe("ago", () => {
  const now = Date.parse("2026-08-01T12:00:00Z");

  it("counts back in the largest unit that fits", () => {
    expect(ago("2026-08-01T11:57:00Z", now)).toContain("3");
    expect(ago("2026-07-31T12:00:00Z", now)).toBe("hier");
  });

  it("returns an empty string rather than NaN for a date it cannot read", () => {
    expect(ago("not a date", now)).toBe("");
  });
});

describe("exact", () => {
  it("gives back the input when it is not a date, instead of `Invalid Date`", () => {
    expect(exact("nope")).toBe("nope");
  });
});

describe("in English", () => {
  beforeEach(() => {
    useUi().locale = "en";
  });

  it("delay uses the English day/hour/minute suffixes", () => {
    expect(delay(50 * 3_600_000)).toBe("+2d02");
    expect(delay(2 * 3_600_000 + 13 * 60_000)).toBe("+2h13");
    expect(delay(45 * 60_000)).toBe("+45min");
  });

  it("ago words a relative time in English", () => {
    const now = Date.parse("2026-08-01T12:00:00Z");
    expect(ago("2026-07-31T12:00:00Z", now)).toBe("yesterday");
  });
});

describe("switching locale at runtime", () => {
  it("does not stick to whichever locale formatted first", () => {
    // The cache is keyed by locale, not a single memoized formatter — a French
    // call, then an English one, both have to come back correct.
    const ui = useUi();
    const now = Date.parse("2026-08-01T12:00:00Z");

    ui.locale = "fr";
    expect(ago("2026-07-31T12:00:00Z", now)).toBe("hier");

    ui.locale = "en";
    expect(ago("2026-07-31T12:00:00Z", now)).toBe("yesterday");

    ui.locale = "fr";
    expect(ago("2026-07-31T12:00:00Z", now)).toBe("hier");
  });
});
