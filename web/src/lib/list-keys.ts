/**
 * Which way a key moves the inbox selection, if any.
 *
 * `j`/`k` only bare: with a modifier they belong to someone else — Ctrl/Cmd+K is
 * the command palette (`AppLayout.vue`), and moving the selection under it
 * opened a row every time the palette did. Typing in a field is never navigation.
 */
export function listStep(event: KeyboardEvent): 1 | -1 | null {
  const target = event.target;
  if (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    (target instanceof HTMLElement && target.isContentEditable)
  ) {
    return null;
  }
  if (event.ctrlKey || event.metaKey || event.altKey) {
    return null;
  }
  if (event.key === "ArrowDown" || event.key === "j") {
    return 1;
  }
  if (event.key === "ArrowUp" || event.key === "k") {
    return -1;
  }
  return null;
}
