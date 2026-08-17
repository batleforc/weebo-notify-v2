/**
 * `en.json` and `fr.json` must translate exactly the same keys — a key added
 * to one and forgotten in the other degrades silently at runtime (vue-i18n
 * falls back rather than erroring), so this is the one place that mismatch is
 * caught at all.
 *
 * Read straight off disk with `path.join`, not `new URL("./locales/…json",
 * import.meta.url)`: `@intlify/unplugin-vue-i18n` (`vite.config.ts`) scans the
 * whole source tree for exactly that shape — its lazy-locale-loading feature —
 * and rewrites it into its own loader wherever it appears, this file included.
 * The rewritten call resolves to something that is not a `file:` URL at all,
 * so `fileURLToPath` throws; splitting the "locales" segment out of the `new
 * URL()` call (or off it entirely, as below) keeps this file invisible to
 * that scan.
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const here = dirname(fileURLToPath(import.meta.url));

function load(name: string): unknown {
  return JSON.parse(readFileSync(join(here, "locales", name), "utf8"));
}

function flatten(value: unknown, prefix = ""): string[] {
  if (typeof value !== "object" || value === null) {
    return [prefix];
  }
  return Object.entries(value as Record<string, unknown>).flatMap(
    ([key, child]) => flatten(child, prefix ? `${prefix}.${key}` : key),
  );
}

describe("locale key parity", () => {
  it("translates exactly the same keys in en.json and fr.json", () => {
    const enKeys = flatten(load("en.json")).sort();
    const frKeys = flatten(load("fr.json")).sort();

    expect(enKeys).toEqual(frKeys);
  });
});
