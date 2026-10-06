/**
 * Bearer-fetched images (avatars, thumbnails) as `blob:` URLs. Ids are immutable,
 * so only memory is bounded (LIMIT, oldest revoked). API refusals cache as `null`;
 * network failures don't.
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
