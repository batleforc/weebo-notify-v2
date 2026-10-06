/** `public/` files are never compiled or linted; pin what a broken manifest/icon loses silently. */

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
    // XML forbids `--` in comments; a broken icon only shows up at install time.
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
