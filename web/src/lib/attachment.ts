/**
 * A notification `image` as an `<img>` src (§2.3, §6.1). A `data_uri` is used as
 * is; an attachment gets a presigned URL (private bucket), not a blob, cached
 * until shortly before the signature expires.
 */

import { presignDownload } from "@/api/generated";
import type { Image } from "@/api/generated/types.gen";

/** Re-sign a minute early: the `<img>` may start loading later. */
const EXPIRY_MARGIN_MS = 60_000;

interface Signed {
  url: string;
  /** Epoch ms after which this entry is not offered again. */
  usableUntil: number;
}

/** Promises are cached so concurrent renders share one request. */
const signed = new Map<string, Promise<Signed | null>>();

/** Never rejects: the caller renders a placeholder instead. */
export function imageSrc(image: Image): Promise<string | null> {
  if (image.kind === "data_uri") {
    return Promise.resolve(image.uri);
  }
  return signedUrl(image.ref);
}

/** Not `async`: the map must be read and written before any yield, or two renders both sign. */
function signedUrl(id: string): Promise<string | null> {
  const known = signed.get(id);
  if (known) {
    return known.then((entry) => {
      if (entry && entry.usableUntil > Date.now()) {
        return entry.url;
      }
      // Expired or failed: drop it and sign once more.
      signed.delete(id);
      return signedUrl(id);
    });
  }

  const pending = sign(id).catch(() => null);
  signed.set(id, pending);
  return pending.then((entry) => {
    if (!entry) {
      // Failures are not cached.
      signed.delete(id);
      return null;
    }
    return entry.url;
  });
}

async function sign(id: string): Promise<Signed | null> {
  const response = await presignDownload({ path: { id } });
  if (response.error || !response.data) {
    return null;
  }

  const expiry = Date.parse(response.data.expires_at);
  return {
    url: response.data.url,
    // Unparseable expiry counts as already stale.
    usableUntil: Number.isNaN(expiry) ? 0 : expiry - EXPIRY_MARGIN_MS,
  };
}

/** Tests only. */
export function forgetAttachments(): void {
  signed.clear();
}
