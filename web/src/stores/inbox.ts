/**
 * The inbox: one page of notifications, kept current by the stream (§8.1).
 * Rows only change through events; unseen arrivals wait in `pending` so the list
 * never jumps under the reader (§10.5).
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

/** A stream arrival lacks revision id, kind and TTL, so those stay optional rather than faked. */
export type Row = Omit<Notification, "head" | "kind" | "ttl"> & {
  head: Omit<Revision, "id"> & { id?: RevisionId };
  kind?: NotificationKind;
  ttl?: Ttl;
};

export interface Filters {
  channels: string[];
  state: State[];
  status: Status[];
  /** AND semantics: a match must carry all of these (§8, `tag` query param). */
  tags: string[];
  minPriority: Priority | null;
  /** `null` = don't filter on action buttons (§2.3). */
  hasActions: boolean | null;
}

export const emptyFilters = (): Filters => ({
  channels: [],
  state: [],
  status: [],
  tags: [],
  minPriority: null,
  hasActions: null,
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

  /** Rows collapsed by `group_key`, headed by the most recent member (§10.4). */
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

  /** Index the reactive proxy read back from `rows`, not the raw object, so later mutations trigger. */
  function remember(row: Row): void {
    if (index.has(row.id)) {
      return;
    }
    rows.value.push(row);
    const stored = rows.value.at(-1);
    if (stored) {
      index.set(stored.id, stored);
    }
  }

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
      tags,
      minPriority,
      hasActions,
    } = filters.value;
    return {
      channels: channels.length ? channels.join(",") : undefined,
      state: state.length ? state.join(",") : undefined,
      status: statuses.length ? statuses.join(",") : undefined,
      // Stored tags are lowercase (`content::validate_tags`).
      tag: tags.length ? tags.map((t) => t.toLowerCase()).join(",") : undefined,
      min_priority: minPriority ?? undefined,
      has_actions: hasActions ?? undefined,
      cursor: after,
      limit: PAGE_SIZE,
    };
  }

  /** Only the latest `load` lands; filter clicks race. */
  let generation = 0;

  /** Events that arrived mid-load, replayed after the page lands (`seq` dedupes). */
  let held: StreamEvent[] = [];

  async function load(): Promise<void> {
    const mine = ++generation;
    loading.value = true;
    error.value = null;
    try {
      const page = await fetchInbox({ query: query(), throwOnError: true });
      if (mine !== generation) {
        return;
      }
      rows.value = page.data.items;
      cursor.value = page.data.next ?? null;
      pending.value = [];
      reindex();
    } catch (caught) {
      if (mine === generation) {
        error.value = failureOf(caught).message;
      }
    } finally {
      if (mine === generation) {
        loading.value = false;
        const replay = held;
        held = [];
        for (const event of replay) {
          ingest(event);
        }
      }
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
        remember(row);
      }
      cursor.value = page.data.next ?? null;
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      loadingMore.value = false;
    }
  }

  async function apply(next: Filters): Promise<void> {
    filters.value = next;
    await load();
    restart();
  }

  /** The stream is only channel-filtered, so the page filters are re-applied here. */
  function matches(revision: RevisionEvent, channel: string): boolean {
    const {
      channels,
      state,
      status: statuses,
      tags,
      minPriority,
      hasActions,
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
    if (tags.length) {
      const carried = revision.payload.tags ?? [];
      if (!tags.every((t) => carried.includes(t.toLowerCase()))) {
        return false;
      }
    }
    if (
      minPriority &&
      PRIORITIES.indexOf(revision.priority) < PRIORITIES.indexOf(minPriority)
    ) {
      return false;
    }
    if (
      hasActions !== null &&
      (revision.payload.actions ?? []).length > 0 !== hasActions
    ) {
      return false;
    }
    return true;
  }

  function applyRevision(event: RevisionEvent, channel: string): void {
    const existing = index.get(event.notification_id);

    if (existing) {
      // `seq` decides, not arrival order: reconnects replay (§2.2).
      if (event.seq < existing.head.seq) {
        return;
      }
      existing.head = { ...existing.head, ...headFrom(event) };
      existing.last_seq = event.seq;
      existing.revision_count = Math.max(existing.revision_count, event.seq);
      sort();
      return;
    }

    if (!matches(event, channel)) {
      return;
    }

    const row = rowFrom(event, channel);
    // Revealed by the separator, so the list never jumps (§10.5).
    pending.value.unshift(row);
  }

  /** Move everything the separator is announcing into the list. */
  function revealPending(): void {
    for (const row of pending.value) {
      remember(row);
    }
    pending.value = [];
    sort();
  }

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
        // receipt/interaction/preference only concern the detail store.
        return;
    }
  }

  /** Callbacks the detail panel registers to see events about what it shows. */
  const listeners = new Set<(event: StreamEvent) => void>();

  function subscribe(listener: (event: StreamEvent) => void): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  /** Call before the first `load`: events during the load are held, not lost. */
  function start(cursor?: string): void {
    stream?.close();
    stream = openStream({
      channels: filters.value.channels,
      cursor,
      onStatus: (value) => {
        status.value = value;
      },
      onEvent: (event) => {
        if (loading.value) {
          held.push(event);
        } else {
          ingest(event);
        }
        for (const listener of listeners) {
          listener(event);
        }
      },
    });
  }

  /** Reopen for new filters, resuming from the old stream's cursor. */
  function restart(): void {
    if (stream) {
      start(stream.lastEventId());
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

/** The revision a stream event describes, minus the id it doesn't carry. */
export function headFrom(event: RevisionEvent): Omit<Revision, "id"> {
  return {
    notification_id: event.notification_id,
    seq: event.seq,
    author: event.author,
    received_at: event.revised_at,
    state: event.state,
    status: event.status,
    priority: event.priority,
    content: event.payload,
  };
}

/** A row from the event alone (§8.1, denormalised): arrivals cost no request. */
function rowFrom(event: RevisionEvent, channel: string): Row {
  return {
    id: event.notification_id,
    channel,
    external_id: event.external_id ?? null,
    head: headFrom(event),
    last_seq: event.seq,
    revision_count: event.seq,
    created_at: event.created_at,
    closed_at: event.state === "closed" ? event.revised_at : null,
    deleted_at: null,
  };
}
