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
