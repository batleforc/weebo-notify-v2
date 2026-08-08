/**
 * Runs before every spec file (`vite.config.ts`'s `test.setupFiles`).
 *
 * `src/config.ts` reads `window.__ENV__` at module-eval time now, not
 * `import.meta.env` (design ADR-0021) — so anything that imports it transitively
 * (most of `src/api`, `src/auth`) needs this defined before that import runs, the
 * same way the old `test.env` block needed `VITE_*` set before Vite inlined it.
 *
 * Fixed, obviously fake, and not loaded from `public/env.js`: a test suite that
 * picks up whatever a developer's local file happens to say is a test suite that
 * passes on one machine. Nothing here is ever contacted — the tests that touch
 * the network mock it.
 */
window.__ENV__ = {
  apiBaseUrl: "https://notify.test",
  oidcIssuer: "https://idp.test",
  oidcClientId: "weebo-notify",
};
