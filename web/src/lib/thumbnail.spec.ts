import { beforeEach, describe, expect, it, vi } from "vitest";

const thumbnail = vi.fn();

vi.mock("@/api/generated", () => ({
  thumbnail: (...args: unknown[]) => thumbnail(...args),
}));

import { forgetObjectUrls } from "./object-url";
import { thumbnailObjectUrl } from "./thumbnail";

/** jsdom has no object URLs, and the point of this module is producing them. */
let minted = 0;
beforeEach(() => {
  forgetObjectUrls();
  thumbnail.mockReset();
  minted = 0;
  vi.stubGlobal("URL", {
    ...URL,
    createObjectURL: () => `blob:thumbnail-${++minted}`,
    revokeObjectURL: () => {},
  });
});

describe("fetching an attachment's thumbnail", () => {
  it("goes through the API client rather than through the img tag", async () => {
    // Header-authenticated route (§5.1): an `<img>` can't fetch it.
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
    // D30: any 404 just means no thumbnail.
    thumbnail.mockResolvedValue({ error: { status: 404 } });

    expect(await thumbnailObjectUrl("att_01J")).toBeNull();
  });

  it("answers null rather than throwing when the request fails outright", async () => {
    thumbnail.mockRejectedValue(new Error("offline"));

    expect(await thumbnailObjectUrl("att_01J")).toBeNull();
  });

  it("asks again after a request that never got an answer", async () => {
    // Offline is not "there is no image": caching that null would leave the row
    // blank until a reload, however long ago the network came back.
    thumbnail.mockRejectedValueOnce(new Error("offline"));
    thumbnail.mockResolvedValue({ data: new Blob(["png"]) });

    expect(await thumbnailObjectUrl("att_01J")).toBeNull();
    expect(await thumbnailObjectUrl("att_01J")).not.toBeNull();
  });
});
