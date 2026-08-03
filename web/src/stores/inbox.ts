/**
 * The inbox: one page of notifications, kept current by the stream.
 *
 * # The rule this store exists to enforce
 *
 * There is exactly one way a notification changes here: an event. The initial page
 * comes from `GET /notifications`, everything after it from `GET /stream` — and
 * both carry the same shapes off the same cursor (§8.1), so "what I missed" and
 * "what is arriving" cannot disagree. Nothing in the UI mutates a row directly;
 * acting on a notification posts an interaction and waits for the event, which is
 * what keeps two open tabs showing the same thing.
 *
 * # Arrivals do not move the ground under the cursor
 *
 * §10.5 is explicit: a new row must not shove the list down while somebody is
 * reading it. So an event about a notification already on screen updates it in
 * place — a status transition is the whole point of the revision model — while an
 * event about a notification we have never seen goes to `pending` and is announced
 * by the "N nouvelles" separator, the pattern every inbox uses.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";
import { inbox as fetchInbox } from "@/api/generated";
import type {
  Notification,
  NotificationKind,
  Priority,
  Revision,
  RevisionEvent,
  RevisionId,
  State,
  Status,
  Event as StreamEvent,
  Ttl,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { openStream, type Stream, type StreamStatus } from "@/api/stream";

/**
 * A row of the inbox.
 *
 * Not quite a `Notification`, and the difference is deliberate. A row built from a
 * page of `GET /notifications` has everything; a row built from a stream arrival
 * has everything §8.1 promises and nothing more — no revision id, no notification
 * kind, no resolved TTL, because a `RevisionEvent` does not carry them. Those
 * three are therefore optional here rather than filled with plausible-looking
 * values: a fabricated id is a bug that surfaces three screens away, and the
 * detail panel fetches the real notification the moment a row is opened anyway.
 */
export type Row = Omit<Notification, "head" | "kind" | "ttl"> & {
  head: Omit<Revision, "id"> & { id?: RevisionId };
  kind?: NotificationKind;
  ttl?: Ttl;
};

/** What the sidebar narrows the inbox to (§10.4). */
export interface Filters {
  channels: string[];
  state: State[];
  status: Status[];
  tag: string | null;
  minPriority: Priority | null;
}

export const emptyFilters = (): Filters => ({
  channels: [],
  state: [],
  status: [],
  tag: null,
  minPriority: null,
});

const PAGE_SIZE = 50;

export const useInbox = defineStore("inbox", () => {
  const rows = ref<Row[]>([]);
  const pending = ref<Row[]>([]);
  const cursor = ref<string | null>(null);
  const loading = ref(false);
  const loadingMore = ref(false);
  const error = ref<string | null>(null);
  const filters = ref<Filters>(emptyFilters());
  const status = ref<StreamStatus>("offline");

  let stream: Stream | null = null;
  const index = new Map<string, Row>();

  const hasMore = computed(() => cursor.value !== null);
  const pendingCount = computed(() => pending.value.length);

  /**
   * Rows collapsed by `group_key` (§10.4).
   *
   * A group is represented by its most recent member and carries the others, so
   * forty revisions of one deploy read as one line that says "et 39 autres"
   * instead of forty lines that say almost the same thing.
   */
  const groups = computed(() => {
    const seen = new Map<string, { head: Row; members: Row[] }>();
    const ordered: { head: Row; members: Row[] }[] = [];

    for (const row of rows.value) {
      const key = row.head.content.group_key;
      if (!key) {
        ordered.push({ head: row, members: [row] });
        continue;
      }
      const existing = seen.get(`${row.channel}/${key}`);
      if (existing) {
        existing.members.push(row);
        continue;
      }
      const group = { head: row, members: [row] };
      seen.set(`${row.channel}/${key}`, group);
      ordered.push(group);
    }

    return ordered;
  });

  function reindex(): void {
    index.clear();
    for (const row of rows.value) {
      index.set(row.id, row);
    }
  }

  /** Most recently revised first — the order `GET /notifications` returns. */
  function sort(): void {
    rows.value.sort(
      (a, b) =>
        Date.parse(b.head.received_at) - Date.parse(a.head.received_at) ||
        b.id.localeCompare(a.id),
    );
  }

  function query(after?: string) {
    const {
      channels,
      state,
      status: statuses,
      tag,
      minPriority,
    } = filters.value;
    return {
      channels: channels.length ? channels.join(",") : undefined,
      state: state.length ? state.join(",") : undefined,
      status: statuses.length ? statuses.join(",") : undefined,
      tag: tag ?? undefined,
      min_priority: minPriority ?? undefined,
      cursor: after,
      limit: PAGE_SIZE,
    };
  }

  /** Load the first page, replacing whatever was there. */
  async function load(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      const page = await fetchInbox({ query: query(), throwOnError: true });
      rows.value = page.data.items;
      cursor.value = page.data.next ?? null;
      pending.value = [];
      reindex();
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      loading.value = false;
    }
  }

  /** Append the next page. A cursor is followed until it is `null` (§8). */
  async function loadMore(): Promise<void> {
    if (!cursor.value || loadingMore.value) {
      return;
    }
    loadingMore.value = true;
    try {
      const page = await fetchInbox({
        query: query(cursor.value),
        throwOnError: true,
      });
      for (const row of page.data.items) {
        if (!index.has(row.id)) {
          rows.value.push(row);
          index.set(row.id, row);
        }
      }
      cursor.value = page.data.next ?? null;
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      loadingMore.value = false;
    }
  }

  /** Replace the filters and reload. */
  async function apply(next: Filters): Promise<void> {
    filters.value = next;
    await load();
    restart();
  }

  /**
   * Does an event belong in the inbox as currently filtered?
   *
   * The server filters the page; the stream is not filtered by anything but the
   * channel list, so the same predicate has to exist here or a filtered inbox
   * would quietly fill up with rows that do not match it.
   */
  function matches(revision: RevisionEvent, channel: string): boolean {
    const {
      channels,
      state,
      status: statuses,
      tag,
      minPriority,
    } = filters.value;
    if (channels.length && !channels.includes(channel)) {
      return false;
    }
    if (state.length && !state.includes(revision.state)) {
      return false;
    }
    if (statuses.length && !statuses.includes(revision.status)) {
      return false;
    }
    if (tag && !(revision.payload.tags ?? []).includes(tag)) {
      return false;
    }
    if (
      minPriority &&
      PRIORITIES.indexOf(revision.priority) < PRIORITIES.indexOf(minPriority)
    ) {
      return false;
    }
    return true;
  }

  /** Fold one revision event into the row it concerns. */
  function applyRevision(event: RevisionEvent, channel: string): void {
    const existing = index.get(event.notification_id);

    if (existing) {
      // Last-write-wins is settled by `seq`, never by arrival order (§2.2): a
      // reconnect can replay an older revision after a newer one has landed.
      if (event.seq < existing.head.seq) {
        return;
      }
      existing.head = {
        ...existing.head,
        seq: event.seq,
        state: event.state,
        status: event.status,
        priority: event.priority,
        author: event.author,
        content: event.payload,
        received_at: event.revised_at,
      };
      existing.last_seq = event.seq;
      existing.revision_count = Math.max(existing.revision_count, event.seq);
      sort();
      return;
    }

    if (!matches(event, channel)) {
      return;
    }

    const row = rowFrom(event, channel);
    // Straight into `pending`: the separator is what puts it on screen, so the
    // list never jumps under a cursor (§10.5).
    pending.value.unshift(row);
  }

  /** Move everything the separator is announcing into the list. */
  function revealPending(): void {
    for (const row of pending.value) {
      if (!index.has(row.id)) {
        rows.value.push(row);
        index.set(row.id, row);
      }
    }
    pending.value = [];
    sort();
  }

  /** Fold one event of any kind. */
  function ingest(event: StreamEvent): void {
    switch (event.event) {
      case "revision": {
        if (event.audience !== "channel") {
          return;
        }
        applyRevision(event, event.channel_id);
        return;
      }
      case "tombstone": {
        const removed = event.notification_id;
        rows.value = rows.value.filter((row) => row.id !== removed);
        pending.value = pending.value.filter((row) => row.id !== removed);
        index.delete(removed);
        return;
      }
      default:
        // `receipt`, `interaction` and `preference` change a notification's
        // detail rather than its place in the list; the detail store subscribes
        // to them on its own.
        return;
    }
  }

  /** Callbacks the detail panel registers to see events about what it shows. */
  const listeners = new Set<(event: StreamEvent) => void>();

  function subscribe(listener: (event: StreamEvent) => void): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  /** Open the stream, narrowed to whatever the filters ask for. */
  function start(): void {
    stream?.close();
    stream = openStream({
      channels: filters.value.channels,
      onStatus: (value) => {
        status.value = value;
      },
      onEvent: (event) => {
        ingest(event);
        for (const listener of listeners) {
          listener(event);
        }
      },
    });
  }

  function restart(): void {
    if (stream) {
      start();
    }
  }

  function stop(): void {
    stream?.close();
    stream = null;
    status.value = "offline";
  }

  return {
    rows,
    groups,
    pending,
    pendingCount,
    cursor,
    hasMore,
    loading,
    loadingMore,
    error,
    filters,
    status,
    apply,
    ingest,
    load,
    loadMore,
    revealPending,
    start,
    stop,
    subscribe,
  };
});

/** Priority as an ordered scale, so `min_priority` means the same thing here. */
const PRIORITIES: Priority[] = ["min", "low", "default", "high", "urgent"];

/**
 * Build a row from a revision event alone.
 *
 * This is what §8.1 means by "denormalised on purpose": everything a row needs to
 * render is in the event, so an arrival costs zero requests. What the event does
 * not carry is left absent — see [`Row`].
 */
function rowFrom(event: RevisionEvent, channel: string): Row {
  return {
    id: event.notification_id,
    channel,
    external_id: event.external_id ?? null,
    head: {
      notification_id: event.notification_id,
      seq: event.seq,
      author: event.author,
      received_at: event.revised_at,
      state: event.state,
      status: event.status,
      priority: event.priority,
      content: event.payload,
    },
    last_seq: event.seq,
    revision_count: event.seq,
    created_at: event.created_at,
    closed_at: event.state === "closed" ? event.revised_at : null,
    deleted_at: null,
  };
}
