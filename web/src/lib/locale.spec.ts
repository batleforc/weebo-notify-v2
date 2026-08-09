import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { detectLocale, LOCALE_KEY } from "./locale";

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("detectLocale", () => {
  it("prefers a stored choice over the browser language", () => {
    localStorage.setItem(LOCALE_KEY, "en");
    vi.stubGlobal("navigator", { language: "fr-FR" });
    expect(detectLocale()).toBe("en");
  });

  it("falls back to the browser language when nothing is stored", () => {
    vi.stubGlobal("navigator", { language: "en-US" });
    expect(detectLocale()).toBe("en");
  });

  it("defaults to French for an unrecognised browser language", () => {
    vi.stubGlobal("navigator", { language: "de-DE" });
    expect(detectLocale()).toBe("fr");
  });

  it("does not throw when navigator has no language at all", () => {
    // e.g. a stubbed `navigator` in another spec's test environment, which
    // mocks only the APIs its own test needs.
    vi.stubGlobal("navigator", {});
    expect(detectLocale()).toBe("fr");
  });
});
