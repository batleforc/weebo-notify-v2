import { beforeEach, describe, expect, it, vi } from "vitest";

const thumbnail = vi.fn();

vi.mock("@/api/generated", () => ({
  thumbnail: (...args: unknown[]) => thumbnail(...args),
}));

import { forgetThumbnails, thumbnailObjectUrl } from "./thumbnail";

/** jsdom has no object URLs, and the point of this module is producing them. */
let minted = 0;
beforeEach(() => {
  forgetThumbnails();
  thumbnail.mockReset();
  minted = 0;
  vi.stubGlobal("URL", {
    ...URL,
    createObjectURL: () => `blob:thumbnail-${++minted}`,
  });
});

describe("fetching an attachment's thumbnail", () => {
  it("goes through the API client rather than through the img tag", async () => {
    // The same bug `lib/avatar.ts` exists to avoid: this route is authenticated
    // by the `Authorization` header alone (§5.1, zero cookies), which an
    // `<img>` cannot send.
    thumbnail.mockResolvedValue({ data: new Blob(["png"]) });

    const url = await thumbnailObjectUrl("att_01J");

    expect(url).toBe("blob:thumbnail-1");
    expect(thumbnail).toHaveBeenCalledWith(
      expect.objectContaining({ path: { id: "att_01J" } }),
    );
  });

  it("asks for the bytes as a blob and not as text", async () => {
    thumbnail.mockResolvedValue({ data: new Blob(["png"]) });

    await thumbnailObjectUrl("att_01J");

    expect(thumbnail).toHaveBeenCalledWith(
      expect.objectContaining({ responseType: "blob" }),
    );
  });

  it("fetches one thumbnail once however many rows show it", async () => {
    thumbnail.mockResolvedValue({ data: new Blob(["png"]) });

    const [first, second] = await Promise.all([
      thumbnailObjectUrl("att_01J"),
      thumbnailObjectUrl("att_01J"),
    ]);

    expect(first).toBe(second);
    expect(thumbnail).toHaveBeenCalledTimes(1);
  });

  it("gives different attachments different urls", async () => {
    thumbnail.mockResolvedValue({ data: new Blob(["png"]) });

    expect(await thumbnailObjectUrl("att_01J")).not.toBe(
      await thumbnailObjectUrl("att_01K"),
    );
  });

  it("answers null rather than throwing when nothing was ever derived", async () => {
    // Design D30: an unsupported source, an upload that never arrived, and a
    // caller who cannot see the notification all answer the same 404 — a row
    // with no thumbnail must not take the rest of the inbox down with it.
    thumbnail.mockResolvedValue({ error: { status: 404 } });

    expect(await thumbnailObjectUrl("att_01J")).toBeNull();
  });

  it("answers null rather than throwing when the request fails outright", async () => {
    thumbnail.mockRejectedValue(new Error("offline"));

    expect(await thumbnailObjectUrl("att_01J")).toBeNull();
  });
});
