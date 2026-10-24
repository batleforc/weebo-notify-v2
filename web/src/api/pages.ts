/**
 * Follow a paged list (§8) to its end, for the views that need every row: console
 * counts, account pickers. Bounded, so a server that keeps answering `next` cannot
 * keep the client looping.
 */

/** 50 pages of 200: far past any one instance's channels or service accounts. */
const MAX_PAGES = 50;
const PAGE_SIZE = 200;

interface Paged<T> {
  items: T[];
  next?: string | null;
}

export async function allPages<T>(
  fetchPage: (query: { cursor?: string; limit: number }) => Promise<Paged<T>>,
): Promise<T[]> {
  const items: T[] = [];
  let cursor: string | undefined;
  for (let page = 0; page < MAX_PAGES; page++) {
    const answer = await fetchPage(
      cursor === undefined
        ? { limit: PAGE_SIZE }
        : { cursor, limit: PAGE_SIZE },
    );
    items.push(...answer.items);
    cursor = answer.next ?? undefined;
    if (cursor === undefined) {
      return items;
    }
  }
  throw new Error(`more than ${MAX_PAGES} pages; refusing to keep following`);
}
