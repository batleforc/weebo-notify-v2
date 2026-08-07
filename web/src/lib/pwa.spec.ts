/**
 * What the browser fetches on install, and nothing in `src/` imports (§10.4, M3).
 *
 * `public/` is copied verbatim into the bundle, so nothing there is compiled,
 * type-checked, linted or imported by a test. That is fine for files nobody reads
 * — and the manifest and its icon are read by an operating system, once, at the
 * moment somebody installs the application, which is the least observable moment
 * this project has. These pin the two properties a broken file loses silently.
 */

import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

function asset(name: string): string {
  return readFileSync(resolve(process.cwd(), "public", name), "utf8");
}

describe("the PWA manifest", () => {
  it("is JSON, and every icon it names exists", () => {
    const manifest = JSON.parse(asset("manifest.webmanifest"));

    expect(manifest.icons.length).toBeGreaterThan(0);
    for (const icon of manifest.icons) {
      expect(() => asset(icon.src.replace(/^\//, ""))).not.toThrow();
    }
  });
});

describe("the application icon", () => {
  it("parses as XML, which is what makes it decodable at all", () => {
    // The bug this exists for, found by opening the page in a real browser
    // (M4quatervicies): the icon's comment mentioned the CSS custom properties
    // `background` and `foreground` by their full names, and **XML forbids a
    // double hyphen inside a comment**. Chrome answered "Download error or
    // resource isn't a valid image", the installed PWA had no icon, and no test
    // anywhere noticed — an `<img>` is the only thing that ever parses this file.
    const svg = asset("icon.svg");
    const parsed = new DOMParser().parseFromString(svg, "image/svg+xml");

    expect(
      parsed.querySelector("parsererror")?.textContent ?? null,
      "public/icon.svg is not well-formed XML, so no browser will decode it",
    ).toBeNull();
    expect(parsed.documentElement.tagName).toBe("svg");
  });

  it("keeps the viewBox an OS needs to scale it to every size", () => {
    // One file for every size is the whole reason there is no raster set, and it
    // only works while the drawing is resolution-independent.
    const parsed = new DOMParser().parseFromString(
      asset("icon.svg"),
      "image/svg+xml",
    );

    expect(parsed.documentElement.getAttribute("viewBox")).toBe("0 0 512 512");
  });
});
