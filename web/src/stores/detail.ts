/**
 * The detail panel: one notification, its timeline, its answers.
 *
 * # The timeline is the product, not a log
 *
 * A notification is not a message but a sequence of revisions (§2.2), so opening
 * one shows what changed and when — `status: info → failure` is the thing an
 * on-call person came to read. That is why the panel fetches `GET /revisions`
 * rather than rendering the head twice.
 *
 * # Acting means posting, never patching
 *
 * `ack`, `reply` and `choice` post an interaction and let the event come back
 * (§2.3). The optimism is limited on purpose: the button locks while the request
 * is in flight and unlocks when the server answers. Drawing an acknowledgement
 * that a `403` is about to take away is worse than a spinner nobody noticed.
 */

import { defineStore } from "pinia";
import { computed, ref } from "vue";

import {
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
  Receipt,
  Revision,
  RevisionId,
  SilenceBody,
  Event as StreamEvent,
} from "@/api/generated/types.gen";
import { failureOf } from "@/api/problem";

/**
 * A row of the timeline.
 *
 * The id is optional for the same reason it is on an inbox row: a revision that
 * arrived on the stream does not have one (§8.1). The timeline keys on `seq`,
 * which is what orders revisions in the first place (§2.2).
 */
export type TimelineRevision = Omit<Revision, "id"> & { id?: RevisionId };

export const useDetail = defineStore("detail", () => {
  const id = ref<string | null>(null);
  const notification = ref<Notification | null>(null);
  const revisions = ref<TimelineRevision[]>([]);
  const interactions = ref<InteractionRecord[]>([]);
  const receipts = ref<Receipt[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);
  /** Set when the notification went away under us — deleted, or expired (§7.2bis). */
  const gone = ref<"deleted" | "expired" | null>(null);
  /** The action key currently in flight, so exactly one button shows a spinner. */
  const acting = ref<string | null>(null);

  const head = computed(() => notification.value?.head ?? null);
  const actions = computed(() => head.value?.content.actions ?? []);

  /** Distinct people who have seen it — the "vu par 3/5" numerator (§7.3). */
  const seenBy = computed(() => {
    const users = new Set<string>();
    for (const entry of receipts.value) {
      if (entry.kind === "seen" && entry.principal.kind === "user") {
        users.add(entry.principal.id);
      }
    }
    return users.size;
  });

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
    receipts.value = [];
    gone.value = null;
    error.value = null;
  }

  /** Open a notification. Safe to call with the same id twice. */
  async function open(next: string): Promise<void> {
    if (id.value === next && notification.value) {
      return;
    }
    id.value = next;
    reset();
    loading.value = true;

    try {
      // One round trip per concern, in parallel: the timeline and the answers are
      // separate endpoints (§8) and neither has to wait for the other.
      const [one, history, answered] = await Promise.all([
        getOne({ path: { id: next }, throwOnError: true }),
        fetchRevisions({
          path: { id: next },
          query: { limit: 100 },
          throwOnError: true,
        }),
        listInteractions({ path: { id: next }, throwOnError: true }),
      ]);

      if (id.value !== next) {
        // Somebody clicked another row while this was in flight.
        return;
      }

      notification.value = one.data;
      revisions.value = [...history.data.items].sort((a, b) => a.seq - b.seq);
      interactions.value = answered.data;
    } catch (caught) {
      const failure = failureOf(caught);
      if (failure.status === 404 || failure.code === "notification.unknown") {
        gone.value = "deleted";
      } else {
        error.value = failure.message;
      }
    } finally {
      loading.value = false;
    }
  }

  function close(): void {
    id.value = null;
    reset();
  }

  /**
   * Tell the server this was read (§7.3).
   *
   * Sent without a device: a browser tab is not an enrolled device in M1, and the
   * receipt model says the count is in users anyway. A channel that does not
   * collect receipts answers `200` with nothing, so this is safe to call blind —
   * but the caller checks the channel policy first to save the request.
   */
  async function markSeen(): Promise<void> {
    const current = id.value;
    if (!current) {
      return;
    }
    try {
      await postReceipt({
        body: { notification_id: current, kind: "seen" },
        throwOnError: true,
      });
    } catch (caught) {
      // A receipt nobody asked for must never interrupt reading.
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
      // `unchanged` is the idempotent ack clicked twice (§2.3, T4) — nothing to
      // add, and nothing to apologise for either.
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

  /**
   * Fold a stream event about the notification on screen.
   *
   * Registered with the inbox store, which owns the single connection: twenty
   * components opening twenty streams would burn the twenty-per-user budget (§8.2)
   * on one browser tab.
   */
  function ingest(event: StreamEvent): void {
    const current = id.value;
    if (!current) {
      return;
    }

    switch (event.event) {
      case "revision": {
        if (event.notification_id !== current) {
          return;
        }
        if (!revisions.value.some((one) => one.seq === event.seq)) {
          revisions.value.push({
            notification_id: event.notification_id,
            seq: event.seq,
            author: event.author,
            received_at: event.revised_at,
            state: event.state,
            status: event.status,
            priority: event.priority,
            content: event.payload,
          });
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
            head: {
              ...notification.value.head,
              seq: event.seq,
              state: event.state,
              status: event.status,
              priority: event.priority,
              author: event.author,
              content: event.payload,
              received_at: event.revised_at,
            },
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
        if (
          event.notification_id === current &&
          !receipts.value.some((one) => one.id === event.id)
        ) {
          receipts.value.push(event);
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
    receipts,
    revisions,
    seenBy,
    silence,
  };
});
