import { describe, expect, it } from "vitest";

import { clientLabel } from "./client-label";

describe("clientLabel", () => {
  it("names the browser and the system", () => {
    expect(
      clientLabel(
        "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Mobile Safari/537.36",
      ),
    ).toBe("Chrome · Android");
    expect(
      clientLabel(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:131.0) Gecko/20100101 Firefox/131.0",
      ),
    ).toBe("Firefox · macOS");
  });

  it("recognises the CLI and falls back to the raw string", () => {
    expect(clientLabel("notify/0.4.0")).toBe("notify CLI");
    expect(clientLabel("curl/8.9")).toBe("curl/8.9");
    expect(clientLabel("")).toBe("?");
  });
});
