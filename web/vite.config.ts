import { fileURLToPath, URL } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import vue from "@vitejs/plugin-vue";
// From `vitest/config` rather than `vite`: it is the same function with the
// `test` block typed, which keeps one config file instead of two.
// `loadEnv` and the plugin type come from vite itself: `vitest/config` re-exports
// `defineConfig` with the `test` block typed, and nothing else.
import { loadEnv, type Plugin } from "vite";
import { defineConfig } from "vitest/config";

// With the extension, which the native config loader Vite is moving to requires.
import { contentSecurityPolicy, inlineScriptHashes } from "./src/lib/csp.ts";

/**
 * Stamp the Content-Security-Policy into the built `index.html` (§10.3, ADR-0019).
 *
 * Build only. `pnpm dev` runs without it, and that is a choice rather than an
 * oversight: Vite's HMR needs a websocket and inline injection the shipped policy
 * must not allow, so a dev-relaxed policy would be a different policy tested in
 * place of the real one. `pnpm build && pnpm preview` serves the real one, and the
 * hashes are checked by the suite rather than by eye.
 */
function csp(mode: string): Plugin {
  return {
    name: "weebo-notify:csp",
    apply: "build",
    transformIndexHtml: {
      // After everything else, so the hashes cover the final document rather
      // than the template it started as.
      order: "post",
      handler(html) {
        // The same two variables `src/config.ts` reads, and the reason the policy
        // can be built at all: a deployment here is a build, so the API and the
        // provider are known now rather than at boot.
        const env = loadEnv(mode, process.cwd(), "VITE_");
        for (const name of ["VITE_API_BASE_URL", "VITE_OIDC_ISSUER"] as const) {
          if (!env[name]) {
            // Failing the build rather than emitting a policy with `undefined`
            // in it: the second one produces a bundle that loads and then
            // cannot reach its own API.
            throw new Error(`${name} is required to build the CSP (§10.3)`);
          }
        }

        return {
          html,
          tags: [
            {
              tag: "meta",
              attrs: {
                "http-equiv": "Content-Security-Policy",
                content: contentSecurityPolicy({
                  apiBaseUrl: env.VITE_API_BASE_URL,
                  oidcIssuer: env.VITE_OIDC_ISSUER,
                  hashes: inlineScriptHashes(html),
                }),
              },
              // Before anything it governs — a meta policy applies to what
              // follows it, so a script above it would be unpoliced.
              injectTo: "head-prepend",
            },
          ],
        };
      },
    },

    /**
     * Check the page that is actually written, not the one that was transformed.
     *
     * The hashes above are computed in a `post` hook, which is the last thing to
     * touch the HTML today. "Today" is the word doing the work: an HTML minifier
     * added to this pipeline later would rewrite the inline script after the
     * policy named it, and the symptom would be a blank page in production and
     * nothing anywhere else. This turns that into a failed build.
     */
    generateBundle(_options, bundle) {
      const page = bundle["index.html"];
      if (page?.type !== "asset") {
        return;
      }

      const html = String(page.source);
      for (const hash of inlineScriptHashes(html)) {
        if (!html.includes(hash.replaceAll("'", ""))) {
          this.error(
            `an inline script in index.html is not covered by the policy (${hash}) — ` +
              "something rewrote the page after the CSP was computed",
          );
        }
      }
    },
  };
}

// The dev server does NOT proxy the API. The browser talks to the server on
// :8080 directly, which is why `config/dev.toml` lists :5173 in `cors_origins`
// (design §5.1): a proxy would make CORS pass in development and fail in
// production, hiding exactly the class of mistake this configuration prevents.
export default defineConfig(({ mode }) => ({
  plugins: [vue(), tailwindcss(), csp(mode)],
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
    // Fixed, obviously fake, and not loaded from any `.env` file: a test suite
    // that picks up whatever `.env.local` happens to say is a test suite that
    // passes on one machine. Nothing here is ever contacted — the tests that
    // touch the network mock it.
    env: {
      VITE_API_BASE_URL: "https://notify.test",
      VITE_OIDC_ISSUER: "https://idp.test",
      VITE_OIDC_CLIENT_ID: "weebo-notify",
    },
    coverage: {
      provider: "v8",
      reporter: ["text-summary", "lcov"],
      reportsDirectory: "coverage",
      // Same honesty rule as the Rust side (design §3.2): generated code is not
      // ours to test, and counting it would inflate the number in the direction
      // that hides gaps in the code we do write.
      exclude: ["src/api/generated/**", "src/main.ts", "**/*.spec.ts"],
    },
  },
}));
