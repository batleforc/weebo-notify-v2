import { beforeEach, describe, expect, it, vi } from "vitest";

const readAvatar = vi.fn();

vi.mock("@/api/generated", () => ({
  readAvatar: (...args: unknown[]) => readAvatar(...args),
}));

import { avatarObjectUrl } from "./avatar";
import { forgetObjectUrls } from "./object-url";

/** jsdom has no object URLs, and the point of this module is producing them. */
let minted = 0;
beforeEach(() => {
  forgetObjectUrls();
  readAvatar.mockReset();
  minted = 0;
  vi.stubGlobal("URL", {
    ...URL,
    createObjectURL: () => `blob:avatar-${++minted}`,
    revokeObjectURL: () => {},
  });
});

describe("fetching an avatar", () => {
  it("goes through the API client rather than through the img tag", async () => {
    // The route needs the `Authorization` header; a plain `<img>` got 401 (§5.1).
    readAvatar.mockResolvedValue({ data: new Blob(["png"]) });

    const url = await avatarObjectUrl("att_01J");

    expect(url).toBe("blob:avatar-1");
    expect(readAvatar).toHaveBeenCalledWith(
      expect.objectContaining({ path: { id: "att_01J" } }),
    );
  });

  it("asks for the bytes as a blob and not as text", async () => {
    // Without this axios decodes an image as a string and the blob is mangled —
    // a broken image that reads like a server problem rather than a client one.
    readAvatar.mockResolvedValue({ data: new Blob(["png"]) });

    await avatarObjectUrl("att_01J");

    expect(readAvatar).toHaveBeenCalledWith(
      expect.objectContaining({ responseType: "blob" }),
    );
  });

  it("fetches one avatar once however many rows show it", async () => {
    // Ids are immutable (§2.4): one request for fifty rows.
    readAvatar.mockResolvedValue({ data: new Blob(["png"]) });

    const [first, second] = await Promise.all([
      avatarObjectUrl("att_01J"),
      avatarObjectUrl("att_01J"),
    ]);

    expect(first).toBe(second);
    expect(readAvatar).toHaveBeenCalledTimes(1);
  });

  it("gives different people different urls", async () => {
    readAvatar.mockResolvedValue({ data: new Blob(["png"]) });

    expect(await avatarObjectUrl("att_01J")).not.toBe(
      await avatarObjectUrl("att_01K"),
    );
  });

  it("answers null rather than throwing when there is no avatar", async () => {
    // A face that does not render must not take a settings page down with it;
    // the fallback initials are already in the template for exactly this.
    readAvatar.mockResolvedValue({ error: { status: 404 } });

    expect(await avatarObjectUrl("att_01J")).toBeNull();
  });

  it("answers null rather than throwing when the request fails outright", async () => {
    readAvatar.mockRejectedValue(new Error("offline"));

    expect(await avatarObjectUrl("att_01J")).toBeNull();
  });

  it("asks again after a request that never got an answer", async () => {
    // Offline is not "there is no image": caching that null would leave the row
    // blank until a reload, however long ago the network came back.
    readAvatar.mockRejectedValueOnce(new Error("offline"));
    readAvatar.mockResolvedValue({ data: new Blob(["png"]) });

    expect(await avatarObjectUrl("att_01J")).toBeNull();
    expect(await avatarObjectUrl("att_01J")).not.toBeNull();
  });
});
