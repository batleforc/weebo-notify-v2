import type { RuntimeEnv } from "@/config";

/**
 * The instance the person chose at first launch (RFC 0001), read from the shell
 * and set as `window.__ENV__` before `config.ts` reads it. `false` when none is
 * chosen yet: the build's own `env.js`, if it baked one, then stands.
 */
export async function adoptChosenInstance(): Promise<boolean> {
  const { invoke } = await import("@tauri-apps/api/core");
  const chosen = await invoke<RuntimeEnv | null>("instance_current");
  if (!chosen) {
    return false;
  }
  window.__ENV__ = chosen;
  return true;
}

/** What `instance_probe` answers: the id to confirm with, and the hosts to name. */
export interface ProbedInstance {
  probe: string;
  api: string;
  issuerHost: string;
}

/** Check a typed address with the shell; nothing is trusted or stored yet. */
export async function probeInstance(url: string): Promise<ProbedInstance> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<ProbedInstance>("instance_probe", { url });
}

/** Ask the person, through the OS's own dialog, to use it; `false` if declined. */
export async function confirmInstance(probe: string): Promise<boolean> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<boolean>("instance_save", { probe });
}

/**
 * "Change server": drop this instance's credential, then the instance itself.
 * Awaited in that order: the credential is filed under the instance's id, and
 * once the instance is gone that id is too.
 */
export async function forgetInstance(): Promise<void> {
  const { clearCredential } = await import("@/auth/credential");
  await clearCredential();
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("instance_clear");
}
