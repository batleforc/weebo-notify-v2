/**
 * What the front end accepts from the shell (§10bis.3quater).
 *
 * The Rust side parses the OS's URL and refuses everything it does not recognise.
 * This is the second gate, and it exists because "the other side checked" stops
 * being true the moment there is a second other side — anything able to emit a Tauri
 * event into this webview reaches this function.
 */

import { describe, expect, it } from "vitest";

import { pathOf } from "./deeplink";

describe("pathOf", () => {
  it("accepts the two permalink shapes of §10.4", () => {
    expect(pathOf({ path: "/n/ntf_01KZDXXHXK4KFHJ8NDR63CA8RK" })).toBe(
      "/n/ntf_01KZDXXHXK4KFHJ8NDR63CA8RK",
    );
    expect(pathOf({ path: "/c/ci-infra" })).toBe("/c/ci-infra");
  });

  it("refuses a payload that is not one", () => {
    expect(pathOf(null)).toBeNull();
    expect(pathOf("/n/ntf_1")).toBeNull();
    expect(pathOf({})).toBeNull();
    expect(pathOf({ path: 42 })).toBeNull();
  });

  it("refuses anything that is not exactly a permalink", () => {
    expect(pathOf({ path: "/admin" })).toBeNull();
    expect(pathOf({ path: "/n/" })).toBeNull();
    expect(pathOf({ path: "n/ntf_1" }), "relative").toBeNull();
    expect(pathOf({ path: "/n/a/b" })).toBeNull();
  });

  it("refuses an absolute URL wearing a path's clothes", () => {
    // `//host/x` is protocol-relative: a router given it would navigate off the
    // app entirely, and it passes any check that only asks "does it start with a
    // slash".
    expect(pathOf({ path: "//evil.example/n/ntf_1" })).toBeNull();
    expect(pathOf({ path: "https://evil.example/n/ntf_1" })).toBeNull();
  });
});
