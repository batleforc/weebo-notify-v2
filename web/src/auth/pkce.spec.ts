import { describe, expect, it } from "vitest";

import {
  base64url,
  beginAuthorization,
  challengeOf,
  randomToken,
  readClaims,
} from "./pkce";

describe("base64url", () => {
  it("uses the URL alphabet and drops the padding", () => {
    // 0xfb 0xff exercises both characters that differ from plain base64.
    expect(base64url(new Uint8Array([0xfb, 0xff]))).toBe("-_8");
    expect(base64url(new Uint8Array([]))).toBe("");
  });
});

describe("challengeOf", () => {
  /** RFC 7636 appendix B: near-misses fail as an opaque `invalid_grant`. */
  it("matches the RFC 7636 test vector", async () => {
    const verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    await expect(challengeOf(verifier)).resolves.toBe(
      "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
    );
  });
});

describe("randomToken", () => {
  it("gives a fresh value each time", () => {
    const tokens = new Set(Array.from({ length: 50 }, () => randomToken(32)));
    expect(tokens.size).toBe(50);
  });

  it("stays inside the length RFC 7636 allows for a verifier", async () => {
    const { pending } = await beginAuthorization("/");
    expect(pending.verifier.length).toBeGreaterThanOrEqual(43);
    expect(pending.verifier.length).toBeLessThanOrEqual(128);
    // The characters an unreserved-only alphabet allows, and nothing else.
    expect(pending.verifier).toMatch(/^[A-Za-z0-9\-._~]+$/);
  });

  it("carries the destination so a permalink survives the round trip", async () => {
    const { pending } = await beginAuthorization("/n/ntf_01J");
    expect(pending.returnTo).toBe("/n/ntf_01J");
    expect(pending.state).not.toBe(pending.verifier);
  });
});

describe("readClaims", () => {
  function jwt(payload: Record<string, unknown>): string {
    // Through UTF-8 bytes, the way a provider actually encodes a payload —
    // `btoa(JSON.stringify(…))` alone throws on the first accented character.
    const encode = (value: object) =>
      btoa(
        String.fromCharCode(...new TextEncoder().encode(JSON.stringify(value))),
      )
        .replace(/\+/g, "-")
        .replace(/\//g, "_")
        .replace(/=+$/, "");
    return `${encode({ alg: "RS256" })}.${encode(payload)}.signature`;
  }

  it("reads the claims the UI draws with", () => {
    const claims = readClaims(
      jwt({
        sub: "alice",
        name: "Alice Martin",
        email: "alice@weebo.local",
        groups: ["infra-admins"],
      }),
    );

    expect(claims).toEqual({
      sub: "alice",
      name: "Alice Martin",
      email: "alice@weebo.local",
      groups: ["infra-admins"],
    });
  });

  it("survives a display name that is not ASCII", () => {
    expect(readClaims(jwt({ sub: "c", name: "Carol Nguyễn" }))?.name).toBe(
      "Carol Nguyễn",
    );
  });

  it("returns null rather than throwing on anything that is not a token", () => {
    expect(readClaims("not-a-token")).toBeNull();
    expect(readClaims("a.b.c")).toBeNull();
    // A well-formed token with no subject is not an identity.
    expect(readClaims(jwt({ name: "nobody" }))).toBeNull();
  });
});
