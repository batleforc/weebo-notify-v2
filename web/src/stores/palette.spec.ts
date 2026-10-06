/** Palette vs. server: stale answers must not win, keystrokes must not each cost a search (§8.2). */

import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Notification } from "@/api/generated/types.gen";
import { DEBOUNCE_MS, usePalette } from "./palette";

const { searchMock, listChannelsMock } = vi.hoisted(() => ({
  searchMock: vi.fn(),
  listChannelsMock: vi.fn(),
}));

vi.mock("@/api/generated", () => ({
  search: searchMock,
  listChannels: listChannelsMock,
}));

function notification(id: string, title: string): Notification {
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
      author: { kind: "user", id: "usr_1", display_name: "Alice" },
      received_at: "2026-08-01T10:00:00Z",
      state: "open",
      status: "info",
      priority: "default",
      content: { title, actions: [], tags: [] },
    },
  } as unknown as Notification;
}

function answers(items: Notification[]) {
  return { data: { items, next: null } };
}

beforeEach(() => {
  setActivePinia(createPinia());
  searchMock.mockReset();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("asking the server", () => {
  it("waits for a pause before turning keystrokes into a request", async () => {
    // §8.2 caps search at 30/min per principal. A request per keystroke burns that
    // in twenty seconds of typing, and the person doing it is not abusing anything.
    searchMock.mockResolvedValue(answers([]));
    const palette = usePalette();

    palette.type("e");
    palette.type("et");
    palette.type("etc");
    palette.type("etcd");
    expect(searchMock).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);

    expect(searchMock).toHaveBeenCalledTimes(1);
    expect(searchMock.mock.calls[0]?.[0]?.query?.q).toBe("etcd");
  });

  it("ignores an older answer that lands after a newer one", async () => {
    // `etc` answering after `etcd` must not overwrite it.
    let resolveFirst: (value: unknown) => void = () => {};
    searchMock
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveFirst = resolve;
          }),
      )
      .mockResolvedValueOnce(
        answers([notification("ntf_new", "etcd degraded")]),
      );

    const palette = usePalette();
    const stale = palette.searchNow("etc");
    const fresh = palette.searchNow("etcd");
    await fresh;

    resolveFirst(answers([notification("ntf_old", "etc something")]));
    await stale;

    expect(palette.results.map((item) => item.id)).toEqual(["ntf_new"]);
  });

  it("does not ask about a single character", async () => {
    // One letter matches half the instance; the palette's local half already
    // answers it, and the server does not need to.
    searchMock.mockResolvedValue(answers([]));
    const palette = usePalette();

    await palette.searchNow("e");

    expect(searchMock).not.toHaveBeenCalled();
  });

  it("keeps switching working when searching fails", async () => {
    // A search that fails must not take the switcher down with it: channels and
    // screens match locally and still have to be reachable.
    searchMock.mockRejectedValue(new Error("boom"));
    const palette = usePalette();

    await palette.searchNow("etcd");

    expect(palette.error).not.toBeNull();
    expect(palette.results).toHaveLength(0);
    expect(palette.items.some((item) => item.id === "screen:settings")).toBe(
      true,
    );
  });
});

describe("opening and closing", () => {
  it("forgets what was typed when it closes", async () => {
    // Reopening onto yesterday's query, with yesterday's results under it, is the
    // small wrongness that makes people stop using a palette.
    searchMock.mockResolvedValue(answers([notification("ntf_1", "etcd")]));
    const palette = usePalette();
    palette.show();
    await palette.searchNow("etcd");
    expect(palette.results).toHaveLength(1);

    palette.hide();

    expect(palette.open).toBe(false);
    expect(palette.query).toBe("");
    expect(palette.results).toHaveLength(0);
  });

  it("drops an answer that lands after it was closed", async () => {
    // Otherwise the palette repopulates itself a second after somebody dismissed
    // it, which looks like it reopened on its own.
    let land: (value: unknown) => void = () => {};
    searchMock.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          land = resolve;
        }),
    );
    const palette = usePalette();
    palette.show();
    const inFlight = palette.searchNow("etcd");

    palette.hide();
    land(answers([notification("ntf_1", "etcd")]));
    await inFlight;

    expect(palette.results).toHaveLength(0);
  });

  it("toggles, because the same shortcut closes it", () => {
    const palette = usePalette();

    palette.toggle();
    expect(palette.open).toBe(true);
    palette.toggle();
    expect(palette.open).toBe(false);
  });
});
