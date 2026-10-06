/**
 * Whether the bundle runs inside the Tauri shell (§10bis.3): one build, asked at
 * runtime. Presentation only — never branch authorisation on it.
 */

/** Declared, not imported: the web build has no Tauri dependency. */
interface MaybeTauri {
  __TAURI_INTERNALS__?: unknown;
  __TAURI__?: unknown;
}

/** The class the shell's rules hang off. */
export const DESKTOP_CLASS = "is-desktop";

/** Checks both globals: Tauri v2 renamed the injected object. */
export function isDesktop(host: unknown = globalThis): boolean {
  const candidate = host as MaybeTauri | null | undefined;
  if (!candidate) {
    return false;
  }
  return (
    candidate.__TAURI_INTERNALS__ !== undefined ||
    candidate.__TAURI__ !== undefined
  );
}

/** A root class rather than reactive state: the gated rules are global and static. */
export function markDesktop(
  root: { classList: { add(name: string): void } } | null,
  host: unknown = globalThis,
): boolean {
  const desktop = isDesktop(host);
  if (desktop && root) {
    root.classList.add(DESKTOP_CLASS);
  }
  return desktop;
}
