import { describe, expect, it, vi } from "vitest";

import { allPages } from "./pages";

describe("allPages", () => {
  it("follows the cursor until it is absent", async () => {
    const fetchPage = vi
      .fn()
      .mockResolvedValueOnce({ items: [1, 2], next: "c1" })
      .mockResolvedValueOnce({ items: [3], next: null });

    expect(await allPages<number>(fetchPage)).toEqual([1, 2, 3]);
    expect(fetchPage).toHaveBeenNthCalledWith(2, { cursor: "c1", limit: 200 });
  });

  it("gives up on a server that never stops paging", async () => {
    const fetchPage = vi.fn().mockResolvedValue({ items: [], next: "again" });

    await expect(allPages(fetchPage)).rejects.toThrow("pages");
    expect(fetchPage).toHaveBeenCalledTimes(50);
  });
});
