import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/config", () => ({ oidcIssuer: "https://idp.test" }));

import { discover, forgetDiscovery } from "./discovery";

function document(overrides: Record<string, unknown> = {}): Response {
  return new Response(
    JSON.stringify({
      issuer: "https://idp.test/",
      authorization_endpoint: "https://idp.test/authorize",
      token_endpoint: "https://idp.test/token",
      ...overrides,
    }),
    { status: 200 },
  );
}

describe("discovery", () => {
  beforeEach(() => forgetDiscovery());
  afterEach(() => vi.unstubAllGlobals());

  it("asks again after a failure instead of failing until a reload", async () => {
    const fetch = vi
      .fn()
      .mockRejectedValueOnce(new TypeError("offline"))
      .mockResolvedValue(document());
    vi.stubGlobal("fetch", fetch);

    await expect(discover()).rejects.toThrow("offline");
    await expect(discover()).resolves.toMatchObject({
      token_endpoint: "https://idp.test/token",
    });
    // …and a success is kept.
    await discover();
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("refuses a document that names another issuer", async () => {
    vi.stubGlobal("fetch", async () =>
      document({ issuer: "https://evil.test" }),
    );

    await expect(discover()).rejects.toThrow("another issuer");
  });

  it("refuses an endpoint outside the issuer's origin", async () => {
    vi.stubGlobal("fetch", async () =>
      document({ end_session_endpoint: "https://evil.test/logout" }),
    );

    await expect(discover()).rejects.toThrow("points outside");
  });
});
