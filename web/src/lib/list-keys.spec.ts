import { describe, expect, it } from "vitest";

import { listStep } from "./list-keys";

const key = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);

/** What `listStep` says about a key pressed inside `element`. */
function inside(element: HTMLElement, init: KeyboardEventInit) {
  let step: ReturnType<typeof listStep> | undefined;
  element.addEventListener("keydown", (event) => {
    step = listStep(event);
  });
  element.dispatchEvent(key(init));
  return step;
}

describe("listStep", () => {
  it("moves on j/k and the arrows", () => {
    expect(listStep(key({ key: "j" }))).toBe(1);
    expect(listStep(key({ key: "ArrowDown" }))).toBe(1);
    expect(listStep(key({ key: "k" }))).toBe(-1);
    expect(listStep(key({ key: "ArrowUp" }))).toBe(-1);
  });

  it("leaves Ctrl/Cmd+K to the command palette", () => {
    expect(listStep(key({ key: "k", ctrlKey: true }))).toBeNull();
    expect(listStep(key({ key: "k", metaKey: true }))).toBeNull();
    expect(listStep(key({ key: "j", altKey: true }))).toBeNull();
  });

  it("never navigates while someone is typing", () => {
    expect(inside(document.createElement("textarea"), { key: "j" })).toBeNull();
    expect(inside(document.createElement("input"), { key: "k" })).toBeNull();
  });
});
