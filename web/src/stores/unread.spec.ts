/** Unread badges against the server (D27, §7.3): dropped channels, burst coalescing, teardown. */

import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { COALESCE_MS, useUnread } from "./unread";

const { unreadMock } = vi.hoisted(() => ({ unreadMock: vi.fn() }));

vi.mock("@/api/generated", () => ({ unread: unreadMock }));

function answers(counts: Record<string, number>) {
  return {
    data: Object.entries(counts).map(([channel, unread]) => ({
      channel,
      unread,
    })),
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  unreadMock.mockReset();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("the unread store", () => {
  it("renders what the server counted, and zero for a channel it did not name", async () => {
    unreadMock.mockResolvedValue(answers({ "ci-infra": 3 }));
    const store = useUnread();

    await store.load();

    expect(store.countOf("ci-infra")).toBe(3);
    // Absent from the answer means nothing unread. The store never holds a zero,
    // so that "no badge" has exactly one representation, as the port promises.
    expect(store.countOf("cluster-ovh-1")).toBe(0);
    expect(store.counts).not.toHaveProperty("cluster-ovh-1");
  });

  it("replaces the previous answer rather than merging into it", async () => {
    const store = useUnread();

    unreadMock.mockResolvedValue(answers({ "ci-infra": 3, backups: 1 }));
    await store.load();
    expect(store.total).toBe(4);

    // `backups` is gone from the second answer, which is how the server says
    // "nothing unread there any more". A merge would leave its badge lit forever.
    unreadMock.mockResolvedValue(answers({ "ci-infra": 1 }));
    await store.load();

    expect(store.countOf("backups")).toBe(0);
    expect(store.total).toBe(1);
  });

  it("sums every channel for the one number a tray or a dock shows", async () => {
    unreadMock.mockResolvedValue(
      answers({ "ci-infra": 3, backups: 1, "cluster-ovh-1": 8 }),
    );
    const store = useUnread();

    await store.load();

    expect(store.total).toBe(12);
  });

  it("turns a burst of events into one request, not one per event", async () => {
    unreadMock.mockResolvedValue(answers({ "ci-infra": 40 }));
    const store = useUnread();

    // Forty stream events must not become forty requests (§8.2).
    for (let index = 0; index < 40; index += 1) {
      store.refresh();
    }
    expect(unreadMock).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(COALESCE_MS);

    expect(unreadMock).toHaveBeenCalledTimes(1);
    expect(store.countOf("ci-infra")).toBe(40);
  });

  it("does not fire a pending refresh once the layout has stopped it", async () => {
    unreadMock.mockResolvedValue(answers({ "ci-infra": 1 }));
    const store = useUnread();

    store.refresh();
    store.stop();
    await vi.advanceTimersByTimeAsync(COALESCE_MS * 4);

    expect(unreadMock).not.toHaveBeenCalled();
  });

  it("keeps the last good count when the server refuses", async () => {
    const store = useUnread();

    unreadMock.mockResolvedValue(answers({ "ci-infra": 2 }));
    await store.load();

    unreadMock.mockRejectedValue(new Error("nope"));
    await store.load();

    // A badge that empties itself on a hiccup reads as "you have read everything",
    // which is a wrong answer where no answer would have done.
    expect(store.countOf("ci-infra")).toBe(2);
    expect(store.error).not.toBeNull();
  });
});
