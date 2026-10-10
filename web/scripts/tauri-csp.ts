/**
 * Print the desktop shell's Content-Security-Policy as a Tauri config merge patch.
 *
 * The desktop bundle's env.js is fixed at build time — there is no nginx to render
 * it at boot — so the API and identity provider it talks to are known here, and
 * the policy names them. The static policy in `tauri.conf.json` is the fallback
 * for a build that skips this, and allows no remote origin at all: such a build
 * fails closed rather than talking to any `https:` host.
 *
 * Inline-script hashes are not passed: Tauri hashes the bundle's inline scripts
 * into the policy itself when it embeds the assets.
 *
 * Used by `.tasks/tauri.yaml` and `.tasks/android.yaml`, as `TAURI_CONFIG` or
 * `--config`. `--updater` (desktop only) merges in the signing and updater settings
 * from the environment (`src/lib/tauri-updater.ts`), and fails the build on a
 * half-configured updater. Run with `node web/scripts/tauri-csp.ts` (Node strips the types).
 */

import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

import { contentSecurityPolicy, originOf } from "../src/lib/csp.ts";
import { updaterConfig } from "../src/lib/tauri-updater.ts";

// `WNK_*` first: it is what `vite.config.ts` (`desktopEnv`) writes into the
// bundle's env.js, so a release build's policy and config name the same origins.
// `public/env.js` (the local dev stack) only for a local `task tauri:build`.
let env: Record<string, string | undefined> | undefined;
if (process.env.WNK_API_BASE_URL) {
  env = {
    apiBaseUrl: process.env.WNK_API_BASE_URL,
    oidcIssuer: process.env.WNK_OIDC_ISSUER,
    attachmentOrigin: process.env.WNK_ATTACHMENT_ORIGIN,
  };
} else {
  const window: { __ENV__?: Record<string, string | undefined> } = {};
  runInNewContext(
    readFileSync(new URL("../public/env.js", import.meta.url), "utf8"),
    { window },
  );
  env = window.__ENV__;
}
if (!env?.apiBaseUrl || !env.oidcIssuer) {
  throw new Error(
    "neither WNK_* nor web/public/env.js set apiBaseUrl and oidcIssuer",
  );
}

const csp = contentSecurityPolicy({
  apiOrigin: originOf(env.apiBaseUrl),
  oidcOrigin: originOf(env.oidcIssuer),
  attachmentOrigin: env.attachmentOrigin && originOf(env.attachmentOrigin),
  hashes: [],
  // The webview's IPC to the Rust side (`invoke`), which is a fetch on Linux and
  // Android.
  connect: ["ipc:", "http://ipc.localhost"],
});

const updater = process.argv.includes("--updater")
  ? updaterConfig(process.env)
  : {};
process.stdout.write(
  JSON.stringify({ app: { security: { csp } }, ...updater }),
);
