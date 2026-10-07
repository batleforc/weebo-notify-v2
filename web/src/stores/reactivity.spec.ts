import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { computed } from "vue";
import type {
  Author,
  Notification,
  Event as StreamEvent,
} from "@/api/generated/types.gen";
import { useInbox } from "./inbox";

/** A revision must reach a `computed` watching the row: the index must hold the reactive proxy. */
const { inboxMock } = vi.hoisted(() => ({ inboxMock: vi.fn() }));
vi.mock("@/api/generated", () => ({ inbox: inboxMock }));
vi.mock("@/api/stream", () => ({ openStream: () => ({ close: () => {} }) }));

const author: Author = { kind: "user", id: "usr_1" };

function notification(id: string): Notification {
  return {
    id,
    channel: "ci-infra",
    kind: "simple",
    external_id: null,
    last_seq: 1,
    revision_count: 1,
    created_at: "2026-08-01T10:00:00Z",
    closed_at: null,
    deleted_at: null,
    ttl: "48h",
    head: {
      id: `rev_${id}`,
      notification_id: id,
      seq: 1,
      author,
      received_at: "2026-08-01T10:00:00Z",
      state: "open",
      status: "info",
      priority: "default",
      content: { title: id },
    },
  };
}

function revision(
  id: string,
  seq: number,
  status: "failure" | "success",
): StreamEvent {
  return {
    cursor: `evt_${seq}`,
    audience: "channel",
    channel_id: "ci-infra",
    logged_at: "2026-08-01T10:05:00Z",
    event: "revision",
    notification_id: id,
    seq,
    state: "closed",
    status,
    priority: "default",
    author,
    created_at: "2026-08-01T10:00:00Z",
    revised_at: "2026-08-01T10:05:00Z",
    payload: { title: id },
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  inboxMock.mockReset();
});

describe("a revision reaches whatever is watching the row", () => {
  it("for a row that arrived with the second page", async () => {
    inboxMock
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_1")], next: "evt_10" },
      })
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_2")], next: null },
      });

    const inbox = useInbox();
    await inbox.load();
    await inbox.loadMore();

    const status = computed(
      () => inbox.rows.find((row) => row.id === "ntf_2")?.head.status,
    );
    expect(status.value).toBe("info");

    inbox.ingest(revision("ntf_2", 2, "failure"));

    expect(status.value).toBe("failure");
  });

  it("for a row revealed from the pending buffer", async () => {
    inboxMock.mockResolvedValue({ data: { items: [], next: null } });
    const inbox = useInbox();
    await inbox.load();

    inbox.ingest(revision("ntf_9", 1, "failure"));
    inbox.revealPending();

    const status = computed(
      () => inbox.rows.find((row) => row.id === "ntf_9")?.head.status,
    );
    expect(status.value).toBe("failure");

    inbox.ingest(revision("ntf_9", 2, "success"));

    expect(status.value).toBe("success");
  });
});
