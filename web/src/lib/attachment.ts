/**
 * Turning a notification's `image` into something an `<img>` can show (§2.3, §6.1).
 *
 * # Why this was missing
 *
 * `image` has been publishable since M1 and servable since M4unvicies, and until
 * now no view rendered the half of it that matters. The detail panel had an
 * `<img>` guarded by `image.kind === 'data_uri'`, so a publisher who attached
 * anything above 48 KiB — the threshold at which the CLI switches to an
 * attachment (§9.1) — watched it disappear silently. That is D26, and it stayed
 * open for one reason: nobody could look at a page.
 *
 * # Two kinds, two paths, and only one of them costs a request
 *
 * A `data_uri` is already the bytes; it goes straight into `src`. An
 * `attachment` is a reference, and §6.1 makes the bucket **strictly private** —
 * a presigned URL is the only way to the object. So this asks
 * `GET /api/v1/attachments/{id}`, which answers with a signed URL and its expiry.
 *
 * # A signed URL, and not a blob
 *
 * Unlike `lib/avatar.ts` next door, which fetches bytes through the API client
 * and hands back an object URL. The difference is not style: an avatar is served
 * *by the server* and needs the `Authorization` header an `<img>` cannot send,
 * while an attachment's signature travels **in the URL** — so it needs no header,
 * and pulling 25 MiB through this tab's memory to produce a blob would undo the
 * reason §6.1 made attachments a client ↔ bucket flow in the first place.
 *
 * # The cache expires, because the signature does
 *
 * Avatars cache for ever and never invalidate — replacing one mints a new id, so
 * an entry cannot go stale. A presign can: it is valid for minutes. An inbox left
 * open longer than that would otherwise re-render a URL the bucket has started
 * refusing, which looks exactly like a broken image. Entries are therefore
 * dropped shortly *before* they expire, so a re-render re-signs rather than
 * failing.
 */

import { presignDownload } from "@/api/generated";
import type { Image } from "@/api/generated/types.gen";

/**
 * How long before a signature's stated expiry an entry stops being reused.
 *
 * A whole minute, which is generous on purpose: the alternative to re-signing a
 * little early is handing a URL to an `<img>` that will start loading it some
 * unknown number of milliseconds later.
 */
const EXPIRY_MARGIN_MS = 60_000;

interface Signed {
  url: string;
  /** Epoch ms after which this entry is not offered again. */
  usableUntil: number;
}

/**
 * Signed URLs by attachment id.
 *
 * The promise is stored rather than the value, so a detail panel and a timeline
 * rendering the same attachment at once make one request instead of racing into
 * two — the same reason `lib/avatar.ts` does it.
 */
const signed = new Map<string, Promise<Signed | null>>();

/**
 * The `src` for an image, or `null` when there cannot be one.
 *
 * Never rejects. A notification whose image cannot be resolved still has a title,
 * a body and a timeline, and none of those should disappear because a bucket is
 * unreachable — the caller renders a placeholder instead.
 */
export function imageSrc(image: Image): Promise<string | null> {
  if (image.kind === "data_uri") {
    return Promise.resolve(image.uri);
  }
  return signedUrl(image.ref);
}

/**
 * Not `async`, deliberately.
 *
 * The map has to be consulted *and written* before the first `await`, or two
 * views rendering the same attachment in the same tick both find it empty and
 * both sign — which is the race this cache exists to prevent, and which an
 * `async` function reintroduces by yielding at the lookup.
 */
function signedUrl(id: string): Promise<string | null> {
  const known = signed.get(id);
  if (known) {
    return known.then((entry) => {
      if (entry && entry.usableUntil > Date.now()) {
        return entry.url;
      }
      // Expired, or a failure kept only long enough for its concurrent callers
      // to share it. Either way this entry is spent; the recursion below takes
      // the fresh branch, so it happens exactly once.
      signed.delete(id);
      return signedUrl(id);
    });
  }

  const pending = sign(id).catch(() => null);
  signed.set(id, pending);
  return pending.then((entry) => {
    if (!entry) {
      // Not remembered: a failure that is cached is a failure that survives the
      // condition that caused it.
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
    // An unparseable expiry is treated as "already stale" rather than as "valid
    // for ever": re-signing costs a request, and showing a dead URL costs the
    // image.
    usableUntil: Number.isNaN(expiry) ? 0 : expiry - EXPIRY_MARGIN_MS,
  };
}

/**
 * Forget every signature. Tests only.
 */
export function forgetAttachments(): void {
  signed.clear();
}
