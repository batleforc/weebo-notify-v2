import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  Author,
  Notification,
  Event as StreamEvent,
} from "@/api/generated/types.gen";
import { emptyFilters, useInbox } from "./inbox";

const { inboxMock } = vi.hoisted(() => ({ inboxMock: vi.fn() }));

vi.mock("@/api/generated", () => ({ inbox: inboxMock }));
vi.mock("@/api/stream", () => ({ openStream: () => ({ close: () => {} }) }));

const author: Author = { kind: "user", id: "usr_1", display_name: "Alice" };

function notification(
  id: string,
  overrides: Partial<Notification> = {},
): Notification {
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
      content: { title: `notification ${id}` },
    },
    ...overrides,
  };
}

function revision(
  id: string,
  overrides: Partial<{
    seq: number;
    channel: string;
    state: "open" | "working" | "closed";
    status: "success" | "failure" | "warning" | "info" | "cancelled";
    priority: "min" | "low" | "default" | "high" | "urgent";
    title: string;
    groupKey: string;
    revisedAt: string;
  }> = {},
): StreamEvent {
  return {
    cursor: `evt_${id}_${overrides.seq ?? 1}`,
    audience: "channel",
    channel_id: overrides.channel ?? "ci-infra",
    logged_at: overrides.revisedAt ?? "2026-08-01T10:05:00Z",
    event: "revision",
    notification_id: id,
    seq: overrides.seq ?? 1,
    state: overrides.state ?? "open",
    status: overrides.status ?? "info",
    priority: overrides.priority ?? "default",
    author,
    created_at: "2026-08-01T10:00:00Z",
    revised_at: overrides.revisedAt ?? "2026-08-01T10:05:00Z",
    payload: {
      title: overrides.title ?? `notification ${id}`,
      group_key: overrides.groupKey,
    },
  };
}

function tombstone(id: string): StreamEvent {
  return {
    cursor: `evt_${id}_x`,
    audience: "channel",
    channel_id: "ci-infra",
    logged_at: "2026-08-01T10:06:00Z",
    event: "tombstone",
    notification_id: id,
    reason: "deleted",
  };
}

async function loadedWith(items: Notification[]) {
  inboxMock.mockResolvedValue({ data: { items, next: null } });
  const inbox = useInbox();
  await inbox.load();
  return inbox;
}

beforeEach(() => {
  setActivePinia(createPinia());
  inboxMock.mockReset();
});

describe("folding stream events into the inbox", () => {
  it("announces an unknown notification instead of inserting it (§10.5)", async () => {
    const inbox = await loadedWith([notification("ntf_1")]);

    inbox.ingest(revision("ntf_2", { title: "etcd degraded" }));

    // The list has not moved under whoever is reading it.
    expect(inbox.rows.map((row) => row.id)).toEqual(["ntf_1"]);
    expect(inbox.pendingCount).toBe(1);

    inbox.revealPending();

    expect(inbox.rows.map((row) => row.id)).toContain("ntf_2");
    expect(inbox.pendingCount).toBe(0);
  });

  it("updates a row already on screen in place — that is the revision model", async () => {
    const inbox = await loadedWith([notification("ntf_1")]);

    inbox.ingest(
      revision("ntf_1", { seq: 2, status: "failure", state: "closed" }),
    );

    const row = inbox.rows[0];
    expect(row.head.status).toBe("failure");
    expect(row.head.state).toBe("closed");
    expect(row.last_seq).toBe(2);
    // No separator: this was not an arrival.
    expect(inbox.pendingCount).toBe(0);
  });

  it("ignores a revision older than the one it already has", async () => {
    // A reconnect replays from a cursor, so an older revision can land after a
    // newer one. `seq` is the arbiter, never arrival order (§2.2).
    const inbox = await loadedWith([notification("ntf_1")]);

    inbox.ingest(revision("ntf_1", { seq: 3, status: "failure" }));
    inbox.ingest(revision("ntf_1", { seq: 2, status: "success" }));

    expect(inbox.rows[0].head.status).toBe("failure");
    expect(inbox.rows[0].last_seq).toBe(3);
  });

  it("removes a tombstoned notification from the list and from the buffer", async () => {
    const inbox = await loadedWith([notification("ntf_1")]);
    inbox.ingest(revision("ntf_2"));

    inbox.ingest(tombstone("ntf_1"));
    inbox.ingest(tombstone("ntf_2"));

    expect(inbox.rows).toHaveLength(0);
    expect(inbox.pendingCount).toBe(0);
  });

  it("does not let the stream widen a filtered inbox", async () => {
    // `GET /stream` is narrowed by channel only, so the other filters have to be
    // applied here or a filtered view fills up with rows that do not match it.
    const inbox = await loadedWith([]);
    await inbox.apply({ ...emptyFilters(), status: ["failure"] });

    inbox.ingest(revision("ntf_9", { status: "success" }));
    expect(inbox.pendingCount).toBe(0);

    inbox.ingest(revision("ntf_9", { status: "failure" }));
    expect(inbox.pendingCount).toBe(1);
  });

  it("keeps a priority floor the way min_priority does", async () => {
    const inbox = await loadedWith([]);
    await inbox.apply({ ...emptyFilters(), minPriority: "high" });

    inbox.ingest(revision("ntf_a", { priority: "default" }));
    inbox.ingest(revision("ntf_b", { priority: "urgent" }));

    expect(inbox.pending.map((row) => row.id)).toEqual(["ntf_b"]);
  });

  it("ignores an event for a channel the inbox is not showing", async () => {
    const inbox = await loadedWith([]);
    await inbox.apply({ ...emptyFilters(), channels: ["ci-infra"] });

    inbox.ingest(revision("ntf_x", { channel: "cluster-ovh-1" }));

    expect(inbox.pendingCount).toBe(0);
  });
});

describe("grouping", () => {
  it("collapses notifications sharing a group_key (§10.4)", async () => {
    const inbox = await loadedWith([
      notification("ntf_1", {
        head: {
          ...notification("ntf_1").head,
          received_at: "2026-08-01T10:10:00Z",
          content: { title: "deploy 1/3", group_key: "deploy-prod" },
        },
      }),
      notification("ntf_2", {
        head: {
          ...notification("ntf_2").head,
          received_at: "2026-08-01T10:09:00Z",
          content: { title: "deploy 2/3", group_key: "deploy-prod" },
        },
      }),
      notification("ntf_3"),
    ]);

    const groups = inbox.groups;

    expect(groups).toHaveLength(2);
    expect(groups[0].members).toHaveLength(2);
    // The group is headed by its most recent member.
    expect(groups[0].head.id).toBe("ntf_1");
  });

  it("does not group across channels, even on the same key", async () => {
    const head = (title: string) => ({
      ...notification("x").head,
      content: { title, group_key: "nightly" },
    });

    const inbox = await loadedWith([
      { ...notification("ntf_1"), channel: "ci-infra", head: head("a") },
      { ...notification("ntf_2"), channel: "cluster-ovh-1", head: head("b") },
    ]);

    expect(inbox.groups).toHaveLength(2);
  });
});

describe("paging", () => {
  it("follows the cursor until it is null, never until a page looks short", async () => {
    inboxMock
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_1")], next: "evt_10" },
      })
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_2")], next: null },
      });

    const inbox = useInbox();
    await inbox.load();
    expect(inbox.hasMore).toBe(true);

    await inbox.loadMore();

    // Appended, not re-sorted: the pages come back in the server's cursor order
    // and re-sorting them client-side would shuffle rows the cursor placed.
    expect(inbox.rows.map((row) => row.id)).toEqual(["ntf_1", "ntf_2"]);
    expect(inbox.hasMore).toBe(false);
  });

  it("does not duplicate a row that both pages contain", async () => {
    inboxMock
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_1")], next: "evt_10" },
      })
      .mockResolvedValueOnce({
        data: { items: [notification("ntf_1")], next: null },
      });

    const inbox = useInbox();
    await inbox.load();
    await inbox.loadMore();

    expect(inbox.rows).toHaveLength(1);
  });
});
