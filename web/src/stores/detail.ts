/**
 * The detail panel: one notification, its revision timeline (§2.2) and answers.
 * Acting posts an interaction and waits for the server; no optimistic UI (§2.3).
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import {
  readership as fetchReadership,
  revisions as fetchRevisions,
  getOne,
  listInteractions,
  receipt as postReceipt,
  silence as postSilence,
  record,
} from "@/api/generated";
import type {
  Interaction,
  InteractionRecord,
  Notification,
  Readership,
  Revision,
  RevisionId,
  SilenceBody,
  Event as StreamEvent,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";
import { headFrom } from "@/stores/inbox";

/** Id optional: stream revisions don't carry one (§8.1). Keyed on `seq`. */
export type TimelineRevision = Omit<Revision, "id"> & { id?: RevisionId };

export const useDetail = defineStore("detail", () => {
  const id = ref<string | null>(null);
  const notification = ref<Notification | null>(null);
  const revisions = ref<TimelineRevision[]>([]);
  const interactions = ref<InteractionRecord[]>([]);
  /** Who read it (§7.3); `null` = channel collects no receipts. Fetched, not stream-accumulated. */
  const readership = ref<Readership | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);
  /** Set when the notification went away under us — deleted, or expired (§7.2bis). */
  const gone = ref<"deleted" | "expired" | null>(null);
  /** The action key currently in flight, so exactly one button shows a spinner. */
  const acting = ref<string | null>(null);

  const head = computed(() => notification.value?.head ?? null);
  const actions = computed(() => head.value?.content.actions ?? []);

  /** Distinct people who have seen it — the "vu par 3/5" numerator (§7.3). */
  const seenBy = computed(() => readership.value?.seen.length ?? 0);

  /** `null` for IdP-group channels, which have no countable audience (§14.2). */
  const audience = computed(() => readership.value?.audience ?? null);

  /** Interactions grouped the way the timeline shows them: oldest first. */
  const answers = computed(() =>
    [...interactions.value].sort(
      (a, b) => Date.parse(a.recorded_at) - Date.parse(b.recorded_at),
    ),
  );

  function reset(): void {
    notification.value = null;
    revisions.value = [];
    interactions.value = [];
    readership.value = null;
    gone.value = null;
    error.value = null;
  }

  /** Only the latest `open`/`close` may write; comparing ids misses A→B→A. */
  let generation = 0;

  /**
   * Events that arrived while the notification was loading: folded in once it has,
   * or the response would overwrite them. Bounded: a load is one round trip.
   */
  let held: StreamEvent[] = [];
  const MAX_HELD = 500;

  /** Open a notification. Safe to call with the same id twice. */
  async function open(next: string): Promise<void> {
    if (id.value === next && notification.value) {
      return;
    }
    const mine = ++generation;
    id.value = next;
    reset();
    held = [];
    loading.value = true;

    try {
      const [one, history, answered, read] = await Promise.all([
        getOne({ path: { id: next }, throwOnError: true }),
        fetchRevisions({
          path: { id: next },
          query: { limit: 100 },
          throwOnError: true,
        }),
        listInteractions({ path: { id: next }, throwOnError: true }),
        fetchReadership({ path: { id: next }, throwOnError: true }),
      ]);

      if (mine !== generation) {
        return;
      }

      notification.value = one.data;
      revisions.value = [...history.data.items].sort((a, b) => a.seq - b.seq);
      interactions.value = answered.data;
      readership.value = read.data ?? null;
      loading.value = false;
      const late = held;
      held = [];
      for (const event of late) {
        ingest(event);
      }
    } catch (caught) {
      if (mine !== generation) {
        return;
      }
      const failure = failureOf(caught);
      if (failure.status === 404 || failure.code === "notification.unknown") {
        gone.value = "deleted";
      } else {
        error.value = failure.message;
      }
    } finally {
      if (mine === generation) {
        loading.value = false;
      }
    }
  }

  function close(): void {
    generation += 1;
    id.value = null;
    loading.value = false;
    reset();
  }

  /** A `receipt` event is only a poke: the server names the person (§2.4). One read in flight. */
  let refreshing = false;
  async function refreshReadership(): Promise<void> {
    const current = id.value;
    if (!current || refreshing) {
      return;
    }
    refreshing = true;
    try {
      const read = await fetchReadership({
        path: { id: current },
        throwOnError: true,
      });
      if (id.value === current) {
        readership.value = read.data ?? null;
      }
    } catch (caught) {
      // A stale count beats an error banner.
      console.debug("readership refused", failureOf(caught).message);
    } finally {
      refreshing = false;
    }
  }

  /** Mark read (§7.3). Safe on channels without receipts, which answer 200 empty. */
  async function markSeen(): Promise<void> {
    const current = id.value;
    if (!current) {
      return;
    }
    try {
      const recorded = await postReceipt({
        body: { notification_id: current, kind: "seen" },
        throwOnError: true,
      });
      // Show our own name without waiting for the stream. `null` = already recorded.
      if (recorded.data) {
        await refreshReadership();
      }
    } catch (caught) {
      // A receipt must never interrupt reading.
      console.debug("seen receipt refused", failureOf(caught).message);
    }
  }

  /** Post an interaction (§2.3). The event brings the record back. */
  async function act(interaction: Interaction, key: string): Promise<void> {
    const current = id.value;
    if (!current || acting.value) {
      return;
    }
    acting.value = key;
    error.value = null;
    try {
      const answer = await record({
        path: { id: current },
        body: interaction,
        throwOnError: true,
      });
      // `unchanged` (idempotent ack, §2.3) has no record.
      const stored = answer.data.record;
      if (stored && !interactions.value.some((one) => one.id === stored.id)) {
        interactions.value.push(stored);
      }
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      acting.value = null;
    }
  }

  /** Stop being notified about this one, for this user only (§2.3, §7.4). */
  async function silence(body: SilenceBody): Promise<void> {
    const current = id.value;
    if (!current) {
      return;
    }
    acting.value = "silence";
    try {
      await postSilence({ path: { id: current }, body, throwOnError: true });
    } catch (caught) {
      error.value = failureOf(caught).message;
    } finally {
      acting.value = null;
    }
  }

  /** Fold a stream event; the inbox store owns the single connection (§8.2). */
  function ingest(event: StreamEvent): void {
    const current = id.value;
    if (!current) {
      return;
    }
    if (loading.value) {
      if (held.length < MAX_HELD) {
        held.push(event);
      }
      return;
    }

    switch (event.event) {
      case "revision": {
        if (event.notification_id !== current) {
          return;
        }
        if (!revisions.value.some((one) => one.seq === event.seq)) {
          revisions.value.push(headFrom(event));
          revisions.value.sort((a, b) => a.seq - b.seq);
        }
        if (notification.value && event.seq >= notification.value.last_seq) {
          notification.value = {
            ...notification.value,
            last_seq: event.seq,
            revision_count: Math.max(
              notification.value.revision_count,
              event.seq,
            ),
            closed_at:
              event.state === "closed"
                ? event.revised_at
                : notification.value.closed_at,
            head: { ...notification.value.head, ...headFrom(event) },
          };
        }
        return;
      }
      case "interaction": {
        if (
          event.notification_id === current &&
          !interactions.value.some((one) => one.id === event.id)
        ) {
          interactions.value.push(event);
        }
        return;
      }
      case "receipt": {
        // `null` readership = receipts off for this channel.
        if (event.notification_id === current && readership.value) {
          void refreshReadership();
        }
        return;
      }
      case "tombstone": {
        if (event.notification_id === current) {
          gone.value = event.reason === "expired" ? "expired" : "deleted";
        }
        return;
      }
      default:
        // `preference` changes silences, which the inbox store owns.
        return;
    }
  }

  return {
    act,
    actions,
    acting,
    answers,
    audience,
    close,
    error,
    gone,
    head,
    id,
    ingest,
    interactions,
    loading,
    markSeen,
    notification,
    open,
    readership,
    revisions,
    seenBy,
    silence,
  };
});
