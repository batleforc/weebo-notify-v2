/**
 * Print the desktop shell's Content-Security-Policy as a Tauri config merge patch.
 *
 * The desktop bundle ships `public/env.js` as-is — there is no nginx to render it
 * at boot — so the API and identity provider it talks to are known at build time,
 * and the policy can name them instead of allowing any `https:` origin. The static
 * policy in `tauri.conf.json` stays the fallback for a build that skips this.
 *
 * Inline-script hashes are not passed: Tauri hashes the bundle's inline scripts
 * into the policy itself when it embeds the assets.
 *
 * Used by `.tasks/tauri.yaml` and `.tasks/android.yaml`, as `TAURI_CONFIG` or
 * `--config`. Run with `node web/scripts/tauri-csp.ts` (Node strips the types).
 */

import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

import { contentSecurityPolicy, originOf } from "../src/lib/csp.ts";

const window: { __ENV__?: Record<string, string | undefined> } = {};
runInNewContext(
  readFileSync(new URL("../public/env.js", import.meta.url), "utf8"),
  { window },
);

const env = window.__ENV__;
if (!env?.apiBaseUrl || !env.oidcIssuer) {
  throw new Error("web/public/env.js sets no apiBaseUrl or oidcIssuer");
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

process.stdout.write(JSON.stringify({ app: { security: { csp } } }));
