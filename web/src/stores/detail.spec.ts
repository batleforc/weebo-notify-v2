/** Readership (§7.3): fetched, not stream-built, and "no policy" is not "nobody". */

import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  Notification,
  Readership,
  Event as StreamEvent,
} from "@/api/generated/types.gen";
import { useDetail } from "./detail";

const {
  getOneMock,
  revisionsMock,
  listInteractionsMock,
  readershipMock,
  receiptMock,
} = vi.hoisted(() => ({
  getOneMock: vi.fn(),
  revisionsMock: vi.fn(),
  listInteractionsMock: vi.fn(),
  readershipMock: vi.fn(),
  receiptMock: vi.fn(),
}));

vi.mock("@/api/generated", () => ({
  getOne: getOneMock,
  revisions: revisionsMock,
  listInteractions: listInteractionsMock,
  readership: readershipMock,
  receipt: receiptMock,
  record: vi.fn(),
  silence: vi.fn(),
}));

const NOTIFICATION: Notification = {
  id: "ntf_1",
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
    id: "rev_1",
    notification_id: "ntf_1",
    seq: 1,
    author: { kind: "user", id: "usr_1", display_name: "Alice" },
    received_at: "2026-08-01T10:00:00Z",
    state: "open",
    status: "info",
    priority: "default",
    content: { title: "etcd degraded" },
  },
};

function roll(overrides: Partial<Readership> = {}): Readership {
  return {
    seen: [
      {
        author: { kind: "user", id: "usr_1", display_name: "Alice Martin" },
        at: "2026-08-01T10:04:00Z",
      },
    ],
    audience: 3,
    delivered_devices: 2,
    ...overrides,
  };
}

function receiptEvent(notification = "ntf_1"): StreamEvent {
  return {
    cursor: "evt_2",
    audience: "channel",
    channel_id: "ci-infra",
    logged_at: "2026-08-01T10:06:00Z",
    event: "receipt",
    id: "rcp_2",
    notification_id: notification,
    principal: { kind: "user", id: "usr_2" },
    kind: "seen",
    at: "2026-08-01T10:06:00Z",
  };
}

/** Open the panel with whatever the readership endpoint is set to answer. */
async function opened() {
  getOneMock.mockResolvedValue({ data: NOTIFICATION });
  revisionsMock.mockResolvedValue({ data: { items: [], next: null } });
  listInteractionsMock.mockResolvedValue({ data: [] });
  const detail = useDetail();
  await detail.open("ntf_1");
  return detail;
}

beforeEach(() => {
  setActivePinia(createPinia());
  for (const mock of [
    getOneMock,
    revisionsMock,
    listInteractionsMock,
    readershipMock,
    receiptMock,
  ]) {
    mock.mockReset();
  }
});

describe("who has read it", () => {
  it("reads the roll from the server rather than from the stream", async () => {
    readershipMock.mockResolvedValue({ data: roll() });

    const detail = await opened();

    // Nobody posted a receipt while this tab was open, and the count is still
    // right — which is the entire reason the endpoint exists.
    expect(readershipMock).toHaveBeenCalledWith(
      expect.objectContaining({ path: { id: "ntf_1" } }),
    );
    expect(detail.seenBy).toBe(1);
    expect(detail.audience).toBe(3);
    expect(detail.readership?.seen[0].author.display_name).toBe("Alice Martin");
  });

  it("keeps 'no policy' and 'nobody' apart (§7.3)", async () => {
    readershipMock.mockResolvedValue({ data: null });

    const detail = await opened();

    expect(detail.readership).toBeNull();
    expect(detail.seenBy).toBe(0);
    expect(detail.audience).toBeNull();
  });

  it("has no denominator on a channel reached through a group (§14.2)", async () => {
    // The server cannot count `infra-admins`, so the badge is "vu par 1", never
    // "vu par 1/1" — which would read as "everybody has seen it".
    readershipMock.mockResolvedValue({ data: roll({ audience: null }) });

    const detail = await opened();

    expect(detail.seenBy).toBe(1);
    expect(detail.audience).toBeNull();
  });

  it("treats a receipt event as a poke and re-reads the roll", async () => {
    readershipMock.mockResolvedValue({ data: roll() });
    const detail = await opened();

    readershipMock.mockResolvedValue({
      data: roll({
        seen: [
          ...roll().seen,
          {
            author: { kind: "user", id: "usr_2", display_name: "Bob" },
            at: "2026-08-01T10:06:00Z",
          },
        ],
      }),
    });
    detail.ingest(receiptEvent());
    await vi.waitUntil(() => detail.seenBy === 2);

    // The frame carries a principal; a hover card needs a person, and only the
    // server can name one (§2.4).
    expect(detail.readership?.seen[1].author.display_name).toBe("Bob");
  });

  it("does not ask again for a channel that collects no receipts", async () => {
    readershipMock.mockResolvedValue({ data: null });
    const detail = await opened();
    readershipMock.mockClear();

    detail.ingest(receiptEvent());

    expect(readershipMock).not.toHaveBeenCalled();
  });

  it("ignores a receipt belonging to another notification", async () => {
    readershipMock.mockResolvedValue({ data: roll() });
    const detail = await opened();
    readershipMock.mockClear();

    detail.ingest(receiptEvent("ntf_other"));

    expect(readershipMock).not.toHaveBeenCalled();
  });

  it("shows your own name without waiting for your event to come back", async () => {
    readershipMock.mockResolvedValue({ data: roll({ seen: [] }) });
    const detail = await opened();
    receiptMock.mockResolvedValue({ data: { id: "rcp_1" } });
    readershipMock.mockResolvedValue({ data: roll() });

    await detail.markSeen();

    expect(detail.seenBy).toBe(1);
  });

  it("does not re-read when the receipt was already there", async () => {
    readershipMock.mockResolvedValue({ data: roll() });
    const detail = await opened();
    readershipMock.mockClear();
    // `null` is the idempotent repost, or a channel with receipts off (§7.3).
    receiptMock.mockResolvedValue({ data: null });

    await detail.markSeen();

    expect(readershipMock).not.toHaveBeenCalled();
  });
});

describe("switching rows while one is loading", () => {
  it("does not let a slow failure for the old row land on the new one", async () => {
    let failOld: (reason: unknown) => void = () => {};
    getOneMock
      .mockReturnValueOnce(new Promise((_, reject) => (failOld = reject)))
      .mockResolvedValue({ data: { ...NOTIFICATION, id: "ntf_2" } });
    revisionsMock.mockResolvedValue({ data: { items: [], next: null } });
    listInteractionsMock.mockResolvedValue({ data: [] });
    readershipMock.mockResolvedValue({ data: null });
    const detail = useDetail();

    const old = detail.open("ntf_1");
    await detail.open("ntf_2");
    failOld(new Error("404"));
    await old;

    expect(detail.gone).toBeNull();
    expect(detail.error).toBeNull();
    expect(detail.loading).toBe(false);
    expect(detail.notification?.id).toBe("ntf_2");
  });
});
