/**
 * Images the API serves itself — avatars and thumbnails — turned into object URLs.
 *
 * Both routes authenticate by the `Authorization` header alone (§5.1, zero
 * cookies), which an `<img src>` cannot send, so the bytes are fetched through the
 * generated client and handed to the page as a `blob:` URL. `lib/avatar.ts` and
 * `lib/thumbnail.ts` say why each one is served this way; this is what they share.
 *
 * # What is cached, and for how long
 *
 * Both ids are immutable (a new upload mints a new id, §2.3, §2.4), so a URL can
 * never go stale. What is bounded is memory: past {@link LIMIT} ids the oldest is
 * dropped and its object URL revoked. An `<img>` already showing it keeps the
 * decoded picture; a row mounted later simply fetches again.
 *
 * A definitive "no image" (an error the API answered) is cached as `null`, so a
 * row with no thumbnail does not ask on every render. A request that never got an
 * answer — offline, a timeout — is not: the next render tries again.
 */

type Read = (
  id: string,
) => Promise<{ data?: unknown; error?: unknown } | undefined>;

/** Object URLs held per cache — a long desktop session is the case this bounds. */
const LIMIT = 500;

const caches: Map<string, Promise<string | null>>[] = [];

/** A cached `id → object URL` lookup over one API read. Never rejects. */
export function objectUrlCache(
  read: Read,
): (id: string) => Promise<string | null> {
  const urls = new Map<string, Promise<string | null>>();
  caches.push(urls);

  return (id) => {
    const known = urls.get(id);
    if (known) {
      return known;
    }

    const pending = fetchAsObjectUrl(read, id).catch(() => {
      if (urls.get(id) === pending) {
        urls.delete(id);
      }
      return null;
    });
    urls.set(id, pending);
    if (urls.size > LIMIT) {
      const [oldest, url] = urls.entries().next().value as [
        string,
        Promise<string | null>,
      ];
      urls.delete(oldest);
      void url.then(revoke);
    }
    return pending;
  };
}

async function fetchAsObjectUrl(
  read: Read,
  id: string,
): Promise<string | null> {
  const response = await read(id);
  if (!response || response.error || !response.data) {
    return null;
  }

  const bytes =
    response.data instanceof Blob
      ? response.data
      : new Blob([response.data as BlobPart]);

  return URL.createObjectURL(bytes);
}

function revoke(url: string | null): void {
  if (url) {
    URL.revokeObjectURL(url);
  }
}

/** Drop and revoke everything — between tests, which share the module. */
export function forgetObjectUrls(): void {
  for (const urls of caches) {
    for (const url of urls.values()) {
      void url.then(revoke);
    }
    urls.clear();
  }
}
