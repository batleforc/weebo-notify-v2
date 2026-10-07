/**
 * `en.json` and `fr.json` must have the same keys (vue-i18n falls back silently).
 * Read via `path.join`, not `new URL(...)`: the i18n plugin rewrites that shape.
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
