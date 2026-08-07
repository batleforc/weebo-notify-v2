import { beforeEach, describe, expect, it, vi } from "vitest";

const download = vi.fn();

vi.mock("@/api/generated", () => ({
  download: (...args: unknown[]) => download(...args),
}));

import { forgetAttachments, imageSrc } from "./attachment";

/** A signature that is comfortably in the future. */
function signedFor(minutes: number, url = "https://s3.test/att_01J?sig=abc") {
  return {
    data: {
      url,
      expires_at: new Date(Date.now() + minutes * 60_000).toISOString(),
    },
  };
}

beforeEach(() => {
  forgetAttachments();
  download.mockReset();
});

describe("resolving a notification image", () => {
  it("hands back a data URI without asking the server anything", async () => {
    const src = await imageSrc({
      kind: "data_uri",
      uri: "data:image/png;base64,AA",
    });

    expect(src).toBe("data:image/png;base64,AA");
    expect(download).not.toHaveBeenCalled();
  });

  it("signs an attachment, which is the half nothing rendered until now", async () => {
    // D26: `image: {kind: "attachment", ref: …}` is what the CLI produces above
    // 48 KiB, and the detail panel's `kind === 'data_uri'` guard matched none of
    // them. The bucket is strictly private (§6.1), so a signature is the only way
    // to the bytes.
    download.mockResolvedValue(signedFor(15));

    const src = await imageSrc({ kind: "attachment", ref: "att_01J" });

    expect(src).toBe("https://s3.test/att_01J?sig=abc");
    expect(download).toHaveBeenCalledWith(
      expect.objectContaining({ path: { id: "att_01J" } }),
    );
  });

  it("signs once for two views showing the same attachment", async () => {
    download.mockResolvedValue(signedFor(15));

    const [first, second] = await Promise.all([
      imageSrc({ kind: "attachment", ref: "att_01J" }),
      imageSrc({ kind: "attachment", ref: "att_01J" }),
    ]);

    expect(first).toBe(second);
    expect(download).toHaveBeenCalledTimes(1);
  });

  it("re-signs rather than reusing a signature about to expire", async () => {
    // The difference from `lib/avatar.ts`, which caches for ever: an avatar id is
    // immutable, a signature is not. An inbox left open past the expiry would
    // otherwise re-render a URL the bucket has begun refusing, which looks exactly
    // like a broken image.
    download.mockResolvedValueOnce(signedFor(0.5, "https://s3.test/first"));
    download.mockResolvedValueOnce(signedFor(15, "https://s3.test/second"));

    await imageSrc({ kind: "attachment", ref: "att_01J" });
    const again = await imageSrc({ kind: "attachment", ref: "att_01J" });

    expect(again).toBe("https://s3.test/second");
    expect(download).toHaveBeenCalledTimes(2);
  });

  it("answers null rather than throwing when the signature cannot be had", async () => {
    // A notification whose image is unreachable still has a title, a body and a
    // timeline, and none of those may disappear with it.
    download.mockResolvedValue({ error: { code: "notification.gone" } });

    expect(await imageSrc({ kind: "attachment", ref: "att_01J" })).toBeNull();
  });

  it("does not remember a failure", async () => {
    // A cached failure outlives the thing that caused it: the bucket comes back
    // and the image stays missing until the tab is reloaded.
    download.mockResolvedValueOnce({ error: { code: "storage.unavailable" } });
    download.mockResolvedValueOnce(signedFor(15));

    expect(await imageSrc({ kind: "attachment", ref: "att_01J" })).toBeNull();
    expect(await imageSrc({ kind: "attachment", ref: "att_01J" })).toBe(
      "https://s3.test/att_01J?sig=abc",
    );
  });

  it("treats an unreadable expiry as stale rather than as eternal", async () => {
    download.mockResolvedValue({
      data: { url: "https://s3.test/att_01J", expires_at: "not a date" },
    });

    await imageSrc({ kind: "attachment", ref: "att_01J" });
    await imageSrc({ kind: "attachment", ref: "att_01J" });

    expect(download).toHaveBeenCalledTimes(2);
  });
});
