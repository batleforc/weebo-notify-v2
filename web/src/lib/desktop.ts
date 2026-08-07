/**
 * Whether this front end is running inside the Tauri shell (§10bis.3, §10bis.3bis).
 *
 * # Why the same bundle asks at runtime rather than being built twice
 *
 * §10bis.3 is explicit that the desktop is "l'empaquetage du front existant" — one
 * application, two containers. Two builds would mean two artefacts to keep in step
 * and a class of bug that only exists in one of them; a runtime question costs one
 * property read at boot and keeps the web and the desktop honest about being the
 * same code.
 *
 * # What it changes, and what it must not
 *
 * §10bis.3bis asks for "zéro artefact navigateur": the desktop must not feel like a
 * site in a window. Everything that follows from this flag is *presentation* —
 * selection, cursors, typography, scrollbars. **No behaviour and no authorisation
 * branch on it**, because a flag an embedder controls is a flag an embedder can lie
 * about, and the web build must never be the lenient one.
 */

/**
 * The shape Tauri injects. Declared rather than imported: the web build has no
 * Tauri dependency at all, and adding one so a type-check could pass would put a
 * desktop package in every browser's bundle.
 */
interface MaybeTauri {
  __TAURI_INTERNALS__?: unknown;
  __TAURI__?: unknown;
}

/** The class the shell's rules hang off. */
export const DESKTOP_CLASS = "is-desktop";

/**
 * Whether the shell is hosting us.
 *
 * Both globals are checked: Tauri v2 renamed the injected object, and a shell
 * pinned to either version has to be recognised — getting this wrong shows up as a
 * desktop app that silently looks like a website, which is precisely the review
 * failure §10bis.3bis names.
 */
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

/**
 * Mark the document so the stylesheet can tell where it is.
 *
 * A class on the root element rather than a media query or a store: the rules it
 * gates are global and static, and a Vue-reactive answer would let a component
 * re-render into browser styling after the shell had already painted.
 */
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
