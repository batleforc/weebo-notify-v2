/**
 * The desktop shell's updater and signing settings, as a Tauri config merge patch.
 *
 * Built from the environment by `scripts/tauri-csp.ts --updater` (`.tasks/tauri.yaml`)
 * and never committed, so a build without the secrets stays a plain unsigned build
 * that never updates — which is what `lib.rs` keys on: it registers the updater
 * plugin only when `plugins.updater` is present.
 *
 * - `TAURI_SIGNING_PRIVATE_KEY` set → `bundle.createUpdaterArtifacts`, so the
 *   bundler signs each artefact (minisign) and writes its `.sig`. Tauri reads the
 *   key (and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) from the environment itself.
 * - `WNK_UPDATER_ENDPOINT` set → `plugins.updater`, with `WNK_UPDATER_PUBKEY` as
 *   the key every downloaded update is verified against.
 *
 * An endpoint without a public key is refused rather than ignored: an updater with
 * no key to check against is a remote code installer, and silently dropping the
 * endpoint would ship a build that someone believes updates and never does.
 */

export type UpdaterEnv = Partial<
  Record<
    "TAURI_SIGNING_PRIVATE_KEY" | "WNK_UPDATER_ENDPOINT" | "WNK_UPDATER_PUBKEY",
    string
  >
>;

export interface UpdaterPatch {
  bundle?: { createUpdaterArtifacts: true };
  plugins?: { updater: { pubkey: string; endpoints: string[] } };
}

export function updaterConfig(env: UpdaterEnv): UpdaterPatch {
  const patch: UpdaterPatch = {};
  if (env.TAURI_SIGNING_PRIVATE_KEY) {
    patch.bundle = { createUpdaterArtifacts: true };
  }

  const endpoint = env.WNK_UPDATER_ENDPOINT?.trim();
  const pubkey = env.WNK_UPDATER_PUBKEY?.trim();
  if (!endpoint) {
    if (pubkey) {
      throw new Error(
        "WNK_UPDATER_PUBKEY is set but WNK_UPDATER_ENDPOINT is not — set both to enable the updater, or neither",
      );
    }
    return patch;
  }
  if (!pubkey) {
    throw new Error(
      "WNK_UPDATER_ENDPOINT is set but WNK_UPDATER_PUBKEY is not — refusing to build an updater that cannot verify what it installs",
    );
  }
  // `{{target}}`/`{{arch}}` placeholders are not valid URL syntax for every
  // parser, so only the scheme is checked: an update fetched over plain HTTP can
  // be withheld or replayed by anyone on the path, signature or not.
  if (!endpoint.startsWith("https://")) {
    throw new Error(`WNK_UPDATER_ENDPOINT must be https://, got ${endpoint}`);
  }
  patch.plugins = { updater: { pubkey, endpoints: [endpoint] } };
  return patch;
}
