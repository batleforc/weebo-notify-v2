import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath, URL } from "node:url";

import vueI18n from "@intlify/unplugin-vue-i18n/vite";
import tailwindcss from "@tailwindcss/vite";
import vue from "@vitejs/plugin-vue";
// From `vitest/config` rather than `vite`: it is the same function with the
// `test` block typed, which keeps one config file instead of two.
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

// With the extension, which the native config loader Vite is moving to requires.
import { contentSecurityPolicy, inlineScriptHashes } from "./src/lib/csp.ts";

/**
 * Emit `csp-template.txt` next to `package.json` — not into `dist/`, which ships
 * inside the image and would otherwise serve this file to anyone who asked
 * (design §10.3, ADR-0021).
 *
 * Build only, and the reason has not changed since ADR-0019: `pnpm dev` runs
 * without a policy at all, because Vite's HMR needs a websocket and inline
 * injection the shipped policy must not allow.
 *
 * What HAS changed is where the policy is enforced. `web/nginx/default.conf.
 * template` renders it as a real header now, at container boot, from
 * `WNK_API_BASE_URL`/`WNK_OIDC_ISSUER`/`WNK_ATTACHMENT_ORIGIN` — so this plugin
 * hands `contentSecurityPolicy` those exact shell placeholders in place of real
 * origins, and only the inline-script hash, which names bytes this build
 * actually shipped, is real. `web/nginx/docker-entrypoint.d/05-runtime-config.sh`
 * is what resolves the placeholders, once, against the container's actual
 * environment.
 *
 * Reads `index.html` back off disk after the build rather than out of Rollup's
 * in-memory bundle — see `closeBundle` below for why.
 */
function cspTemplate(): Plugin {
  let outDir = "dist";

  return {
    name: "weebo-notify:csp-template",
    apply: "build",

    // `build.outDir` is configurable and this plugin must not assume the
    // default — `closeBundle` below needs the real path to read what was
    // actually written.
    configResolved(config) {
      outDir = config.build.outDir;
    },

    /**
     * Read the page that is actually on disk, then emit the template from it.
     *
     * `closeBundle` runs after every file — HTML included — has been written,
     * which `generateBundle`'s in-memory bundle does not reliably hold `index.
     * html` in (Rolldown-vite writes it through a separate path). Reading the
     * real artefact side-steps more than that mismatch: ADR-0019's version of
     * this hook computed the hash from an intermediate snapshot and then
     * checked it was still findable, quoted-free, inside a `<meta>` tag it had
     * just written into the same snapshot — catching a minifier that ran between
     * the two and rewrote the script out from under an already-decided hash.
     * There is no `<meta>` tag anymore (ADR-0021) and nothing left to compare
     * against: this reads the exact bytes that ship, at the last possible
     * moment, so a hash computed from a stale snapshot is not a failure mode
     * this hook can have.
     */
    closeBundle() {
      const html = readFileSync(resolve(outDir, "index.html"), "utf8");
      const hashes = inlineScriptHashes(html);

      // `csp.spec.ts` pins this at exactly one — the pre-paint theme switch.
      // Zero means `script-src 'self'` ships with no inline allowance at all,
      // silently blocking that script in production; the failure a stray build
      // step deleting or bundling it away deserves is here, not a blank page
      // discovered later.
      if (hashes.length === 0) {
        this.error(
          "index.html has no inline script for the policy to name — " +
            "something removed the pre-paint theme switch, or bundled it away",
        );
      }

      // Shell placeholders, deliberately not JS template strings — this text is
      // never interpolated here, only written out for `envsubst` to resolve
      // later (see this function's own doc comment).
      const template = contentSecurityPolicy({
        // biome-ignore lint/suspicious/noTemplateCurlyInString: shell syntax, resolved by envsubst at container boot, not by JS
        apiOrigin: "${WNK_API_BASE_URL}",
        // biome-ignore lint/suspicious/noTemplateCurlyInString: shell syntax, resolved by envsubst at container boot, not by JS
        oidcOrigin: "${WNK_OIDC_ISSUER}",
        // biome-ignore lint/suspicious/noTemplateCurlyInString: shell syntax, resolved by envsubst at container boot, not by JS
        attachmentOrigin: "${WNK_ATTACHMENT_ORIGIN}",
        hashes,
      });
      writeFileSync(resolve(process.cwd(), "csp-template.txt"), template);
    },
  };
}

// The dev server does NOT proxy the API. The browser talks to the server on
// :8080 directly, which is why `config/dev.toml` lists :5173 in `cors_origins`
// (design §5.1): a proxy would make CORS pass in development and fail in
// production, hiding exactly the class of mistake this configuration prevents.
// A plain object, not a function: nothing here depends on the mode or env
// anymore now that `cspTemplate` needs neither (ADR-0021).
export default defineConfig({
  plugins: [
    vue(),
    tailwindcss(),
    cspTemplate(),
    vueI18n({
      // `.json` only: `src/locales/**` also holds `keys.spec.ts`, which this
      // plugin would otherwise try (and fail) to parse as a locale resource.
      include: [
        fileURLToPath(new URL("./src/locales/**/*.json", import.meta.url)),
      ],
    }),
  ],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: {
    port: 5173,
    // Eclipse Che routes ports it sees a process listening on, and it only sees
    // the ones bound outside the loopback interface (§12.4).
    host: true,
    allowedHosts: ["localhost", ".cde.batleforc.fr"],
  },
  test: {
    // jsdom rather than the faster happy-dom, and the reason is worth keeping:
    // DOMPurify reports `isSupported` under happy-dom but sanitises incorrectly
    // there — it kept an `<img>` while dropping the `<p>` around it, and gave
    // different answers for the same input. A security test that passes against a
    // sanitiser behaving differently from the browser's proves nothing, so the
    // suite runs on the DOM DOMPurify is tested against.
    environment: "jsdom",
    include: ["src/**/*.spec.ts"],
    // `src/config.ts` reads `window.__ENV__`, not `import.meta.env`, so this
    // replaces the fixed `VITE_*` values the old `env:` block set here.
    setupFiles: ["./src/test-setup.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text-summary", "lcov"],
      reportsDirectory: "coverage",
      // Same honesty rule as the Rust side (design §3.2): generated code is not
      // ours to test, and counting it would inflate the number in the direction
      // that hides gaps in the code we do write.
      exclude: [
        "src/api/generated/**",
        "src/main.ts",
        "src/test-setup.ts",
        "**/*.spec.ts",
      ],
    },
  },
});
