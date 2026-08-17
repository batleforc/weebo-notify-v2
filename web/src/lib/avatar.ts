/**
 * Getting an avatar onto the page (design §2.4, §5.1).
 *
 * # Why this exists rather than `<img src="/api/v1/avatars/{id}">`
 *
 * That is what the settings page did, and it could never have worked. The route
 * is authenticated — its credential comes from the `Authorization` header and
 * from nowhere else — and design §5.1 keeps **zero cookies** so that there is no
 * CSRF surface. A browser issuing an `<img>` request sends neither, so every
 * avatar in this application answered `401` and rendered as a broken image.
 *
 * Avatars are the one upload the server serves itself (§2.4, §6.1): the bytes
 * come *through* the API rather than from a presigned bucket URL, precisely so
 * they can be cached hard and rendered on every row of an inbox without a
 * signing round trip. That decision is still the right one — what was missing is
 * that fetching them has to go through the same client as every other call, so
 * the bearer token and its refresh interceptor apply.
 *
 * Attachment images are deliberately *not* handled here: `GET /attachments/{id}`
 * answers with a **presigned** URL, and a signature travels in the URL, so those
 * can go straight into an `<img src>` with no fetch and no blob.
 */

import { readAvatar } from "@/api/generated";

/**
 * Object URLs by attachment id, so a hundred rows showing one person fetch once.
 *
 * Safe to cache for ever and never invalidate, which is not a shortcut but the
 * property §2.4 designed in: replacing an avatar **mints a new id**, so an entry
 * here can never be stale. The route says the same thing in its `cache-control:
 * immutable`.
 *
 * The promise is stored rather than the resolved URL, so that two rows rendering
 * at once share one request instead of racing into two.
 *
 * What this costs, stated rather than discovered: the object URLs live until the
 * page is unloaded. The bound is the number of *distinct* avatars a session
 * looks at, and each is a 256×256 image (§2.4) — revoking them would mean
 * reference-counting every `<img>` that might still be showing one, which is a
 * lot of machinery to reclaim a few hundred kilobytes on a tab someone is about
 * to close.
 */
const urls = new Map<string, Promise<string | null>>();

/**
 * The object URL for an avatar, or `null` when it cannot be had.
 *
 * Never rejects. A missing or unreadable avatar is a face that does not render —
 * the fallback initials take over — and it must not take a settings page or an
 * inbox row down with it.
 */
export function avatarObjectUrl(id: string): Promise<string | null> {
  const known = urls.get(id);
  if (known) {
    return known;
  }

  const pending = fetchAsObjectUrl(id).catch(() => null);
  urls.set(id, pending);
  return pending;
}

async function fetchAsObjectUrl(id: string): Promise<string | null> {
  // Through the generated client, which is the whole point: it carries the
  // bearer token and retries once behind the refresh interceptor.
  const response = await readAvatar({
    path: { id },
    // The bytes are an image, not JSON. Without this axios decodes them as text
    // and the blob is a mangled string — a broken image that looks like a server
    // problem rather than a client one.
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
export function forgetAvatars(): void {
  urls.clear();
}
