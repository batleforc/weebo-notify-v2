/**
 * Getting a row's attachment thumbnail onto the page (design §2.3, §10.3, D30).
 *
 * # Why this is not `lib/attachment.ts`
 *
 * That module resolves the *full* image, and for an `attachment` it deliberately
 * asks the bucket directly: `GET /attachments/{id}` answers a presigned URL, so
 * the bytes never pass through this tab, which is the whole point for something
 * up to 25 MiB.
 *
 * A thumbnail is the opposite case. It is served *through* the API rather than
 * presigned — the same reason `lib/avatar.ts` gives for the avatar: it renders
 * next to every visible row of what may be a virtualised list, and a signature
 * per render would mean a signing round trip per row. Served through the API
 * means it needs the `Authorization` header, which an `<img src>` cannot send
 * (design keeps zero cookies, §5.1) — so this fetches through the generated
 * client, exactly like `lib/avatar.ts`, and hands back an object URL.
 *
 * # The cache never expires, and that is not a shortcut
 *
 * An attachment is never replaced in place: a revision showing a different
 * image references a different attachment id (design §2.3). So this id's
 * thumbnail cannot become a different image later, which is the same property
 * that lets avatars cache for ever — and the route's own `cache-control:
 * immutable` says so too.
 */

import { thumbnail } from "@/api/generated";

/**
 * Object URLs by attachment id.
 *
 * The promise is stored rather than the resolved URL, for the same reason
 * `lib/avatar.ts` does it: two rows rendering the same attachment at once share
 * one request instead of racing into two.
 */
const urls = new Map<string, Promise<string | null>>();

/**
 * The object URL for an attachment's thumbnail, or `null` when there is none.
 *
 * Never rejects. Nothing was necessarily ever derived for this id — an
 * unsupported source format, an upload that never arrived, or a deployment with
 * no object store all answer the same way (design D30) — and a row with no
 * thumbnail must not take the rest of the inbox down with it.
 */
export function thumbnailObjectUrl(id: string): Promise<string | null> {
  const known = urls.get(id);
  if (known) {
    return known;
  }

  const pending = fetchAsObjectUrl(id).catch(() => null);
  urls.set(id, pending);
  return pending;
}

async function fetchAsObjectUrl(id: string): Promise<string | null> {
  const response = await thumbnail({
    path: { id },
    // The bytes are an image, not JSON — see `lib/avatar.ts` for what axios does
    // to a blob without this.
    responseType: "blob",
  });

  if (response.error || !response.data) {
    return null;
  }

  const bytes =
    response.data instanceof Blob
      ? response.data
      : new Blob([response.data as BlobPart]);

  return URL.createObjectURL(bytes);
}

/**
 * Forget everything. Tests only — the cache is deliberately permanent in a live
 * page, so nothing in the application calls this.
 */
export function forgetThumbnails(): void {
  urls.clear();
}
