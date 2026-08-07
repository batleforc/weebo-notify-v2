/**
 * The one flag that decides whether the app dresses as a desktop app (§10bis.3bis).
 *
 * Worth its own tests despite being ten lines: getting it wrong in one direction
 * ships a desktop app that looks like a website — the review failure §10bis.3bis
 * names — and in the other dresses a browser tab as something it is not.
 */

import { describe, expect, it, vi } from "vitest";

import { DESKTOP_CLASS, isDesktop, markDesktop } from "./desktop";

describe("isDesktop", () => {
  it("recognises both globals Tauri has injected", () => {
    // v2 renamed the injected object. A shell pinned to either has to be
    // recognised, and the failure is silent: the app just looks like a website.
    expect(isDesktop({ __TAURI_INTERNALS__: {} })).toBe(true);
    expect(isDesktop({ __TAURI__: {} })).toBe(true);
  });

  it("says no in a plain browser", () => {
    expect(isDesktop({})).toBe(false);
  });

  it("says no rather than throwing when there is no host at all", () => {
    // This runs at boot, before anything else. Throwing here would be a white
    // screen, and the honest answer for "no host" is "not a desktop".
    expect(isDesktop(null)).toBe(false);
    expect(isDesktop(undefined)).toBe(false);
  });
});

describe("markDesktop", () => {
  it("marks the document so the stylesheet can tell where it is", () => {
    const add = vi.fn();
    const marked = markDesktop({ classList: { add } }, { __TAURI__: {} });

    expect(marked).toBe(true);
    expect(add).toHaveBeenCalledWith(DESKTOP_CLASS);
  });

  it("leaves a browser document untouched", () => {
    const add = vi.fn();

    expect(markDesktop({ classList: { add } }, {})).toBe(false);
    // Not "adds a different class": the web build is the default, and styling it
    // through the absence of a class keeps one of the two paths free of rules.
    expect(add).not.toHaveBeenCalled();
  });

  it("does not need a document to answer", () => {
    // The shell asks this before the first paint (§10bis.3bis), which is early
    // enough that a caller may legitimately have nothing to mark yet.
    expect(markDesktop(null, { __TAURI__: {} })).toBe(true);
  });
});
