import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

/**
 * Read off disk rather than imported.
 *
 * `@intlify/unplugin-vue-i18n` owns `src/locales/**` in this Vite pipeline and
 * pre-compiles messages at build time — including through `?raw` — so any import
 * yields a compiled AST (`{ body: { items: [...] } }`) rather than the strings a
 * translator wrote. Walking that would assert something about the compiler's
 * output shape, which differs per language for anything interpolated. Vitest runs
 * with `web/` as its working directory, so a plain relative read gets the source
 * file, which is the artefact under review.
 */
function read(name: string): unknown {
  return JSON.parse(readFileSync(`src/locales/${name}.json`, "utf8"));
}

const en = read("en");
const fr = read("fr");

/**
 * The locale files carry text, and only text.
 *
 * This used to be a convention held up by four comments reading "static,
 * locale-file text only". It was true, and it was true only for as long as every
 * `v-html` interpolation stayed a literal — the components render through
 * `<i18n-t>` slots now, so markup lives in the template and a translation is
 * inert data.
 *
 * The test exists because that property has to survive the day this project
 * accepts a community translation. Reviewers read code diffs carefully and locale
 * diffs quickly, which is exactly what makes a locale file an attractive place to
 * put a `<script>` — and exactly why the guarantee should not depend on anybody
 * noticing.
 */

const locales: Array<[string, unknown]> = [
  ["en", en],
  ["fr", fr],
];

/** Every leaf string, with the dotted path that reaches it. */
function* leaves(value: unknown, path = ""): Generator<[string, string]> {
  if (typeof value === "string") {
    yield [path, value];
    return;
  }
  if (value && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      yield* leaves(child, path ? `${path}.${key}` : key);
    }
  }
}

describe.each(locales)("%s.json", (_name, messages) => {
  const entries = [...leaves(messages)];

  it("has messages to check", () => {
    expect(entries.length).toBeGreaterThan(50);
  });

  it("contains no HTML tags", () => {
    const offenders = entries.filter(([, text]) =>
      /<\/?[a-z][\s\S]*?>/i.test(text),
    );
    expect(offenders).toEqual([]);
  });

  it("contains no entity escapes that would decode into markup", () => {
    // `&lt;script&gt;` is inert as text and becomes markup the moment anything
    // renders it as HTML. Nothing does today; this keeps it that way.
    const offenders = entries.filter(([, text]) =>
      /&(lt|gt|#x?\d*3[cCeE]);/i.test(text),
    );
    expect(offenders).toEqual([]);
  });

  it("contains no javascript: or data: URLs", () => {
    const offenders = entries.filter(([, text]) =>
      /(javascript|data|vbscript)\s*:/i.test(text),
    );
    expect(offenders).toEqual([]);
  });
});

describe("locale parity", () => {
  it("defines the same keys in every locale", () => {
    // A missing key falls back silently and renders the other language; a stray
    // one is usually the leftover of a string somebody stopped using.
    const keys = locales.map(([name, messages]) => [
      name,
      new Set([...leaves(messages)].map(([path]) => path)),
    ]) as Array<[string, Set<string>]>;

    const [[firstName, first], ...rest] = keys;
    for (const [name, other] of rest) {
      expect({
        [`missing from ${name}`]: [...first]
          .filter((k) => !other.has(k))
          .sort(),
        [`missing from ${firstName}`]: [...other]
          .filter((k) => !first.has(k))
          .sort(),
      }).toEqual({
        [`missing from ${name}`]: [],
        [`missing from ${firstName}`]: [],
      });
    }
  });
});
