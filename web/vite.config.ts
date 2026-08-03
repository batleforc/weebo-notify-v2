import { fileURLToPath, URL } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import vue from "@vitejs/plugin-vue";
// From `vitest/config` rather than `vite`: it is the same function with the
// `test` block typed, which keeps one config file instead of two.
import { defineConfig } from "vitest/config";

// The dev server does NOT proxy the API. The browser talks to the server on
// :8080 directly, which is why `config/dev.toml` lists :5173 in `cors_origins`
// (design §5.1): a proxy would make CORS pass in development and fail in
// production, hiding exactly the class of mistake this configuration prevents.
export default defineConfig({
  plugins: [vue(), tailwindcss()],
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
});
