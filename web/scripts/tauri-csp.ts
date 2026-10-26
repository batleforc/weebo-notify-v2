/**
 * Print the desktop shell's Content-Security-Policy as a Tauri config merge patch.
 *
 * A build may preset an instance (`WNK_*`, managed deployments): the policy then
 * names its API and identity provider. Without one (RFC 0001) nothing is printed
 * for the policy, so the static one in `tauri.conf.json` stands — it allows no
 * remote origin — and the shell adds the origins of the instance the person
 * chooses at first launch (`web/src-tauri/src/instance.rs`, `csp_for`). There is
 * deliberately no fallback to `web/public/env.js`: those are the dev stack's
 * localhost values, which a bundle must never ship.
 *
 * Inline-script hashes are not passed: Tauri hashes the bundle's inline scripts
 * into the policy itself when it embeds the assets.
 *
 * Used by `.tasks/tauri.yaml` and `.tasks/android.yaml`, as `TAURI_CONFIG` or
 * `--config`. `--updater` (desktop only) merges in the signing and updater settings
 * from the environment (`src/lib/tauri-updater.ts`), and fails the build on a
 * half-configured updater. Run with `node web/scripts/tauri-csp.ts` (Node strips the types).
 */

import { contentSecurityPolicy, originOf } from "../src/lib/csp.ts";
import { updaterConfig } from "../src/lib/tauri-updater.ts";

const apiBaseUrl = process.env.WNK_API_BASE_URL;
const oidcIssuer = process.env.WNK_OIDC_ISSUER;
if (apiBaseUrl && !oidcIssuer) {
  throw new Error("WNK_API_BASE_URL is set but WNK_OIDC_ISSUER is not");
}

const security = apiBaseUrl
  ? {
      app: {
        security: {
          csp: contentSecurityPolicy({
            apiOrigin: originOf(apiBaseUrl),
            oidcOrigin: originOf(oidcIssuer ?? ""),
            attachmentOrigin:
              process.env.WNK_ATTACHMENT_ORIGIN &&
              originOf(process.env.WNK_ATTACHMENT_ORIGIN),
            hashes: [],
            // The webview's IPC to the Rust side (`invoke`), which is a fetch on
            // Linux and Android.
            connect: ["ipc:", "http://ipc.localhost"],
          }),
        },
      },
    }
  : {};

const updater = process.argv.includes("--updater")
  ? updaterConfig(process.env)
  : {};
process.stdout.write(JSON.stringify({ ...security, ...updater }));
