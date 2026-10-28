/** The inbox list (§10.4): groups stay reachable, keys walk what is shown, and each empty state reads as itself. */
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick, reactive } from "vue";
import { createI18n } from "vue-i18n";

import en from "@/locales/en.json";
import { emptyFilters, type Row } from "@/stores/inbox";
import InboxView from "./InboxView.vue";

const openedAt = Date.now();

function row(
  id: string,
  options: {
    groupKey?: string;
    head?: Record<string, unknown>;
    content?: Record<string, unknown>;
    lastSeq?: number;
  } = {},
): Row {
  return {
    id,
    channel: "news",
    last_seq: options.lastSeq ?? 1,
    revision_count: options.lastSeq ?? 1,
    head: {
      seq: options.lastSeq ?? 1,
      state: "closed",
      status: "info",
      priority: "default",
      received_at: new Date(openedAt - 60_000).toISOString(),
      author: { kind: "service_account", id: "bot", display_name: "Bot" },
      content: {
        title: `title ${id}`,
        group_key: options.groupKey,
        ...options.content,
      },
      ...options.head,
    },
  } as unknown as Row;
}

type Group = { head: Row; members: Row[] };

function group(...members: Row[]): Group {
  return { head: members[0] as Row, members };
}

const digest = () =>
  group(
    row("ntf_c", { groupKey: "digest" }),
    row("ntf_b", { groupKey: "digest" }),
    row("ntf_a", { groupKey: "digest" }),
  );

const inbox = reactive({
  filters: emptyFilters(),
  groups: [] as Group[],
  loading: false,
  error: null as string | null,
  pendingCount: 0,
  hasMore: false,
  loadingMore: false,
  load: vi.fn(),
  loadMore: vi.fn(),
  revealPending: vi.fn(),
});
const detail = reactive({ id: null as string | null });
const route = reactive({ params: {} as Record<string, string> });
const router = { push: vi.fn(), replace: vi.fn() };

vi.mock("@/stores/inbox", async (original) => ({
  ...(await original<typeof import("@/stores/inbox")>()),
  useInbox: () => inbox,
}));
vi.mock("@/stores/channels", () => ({
  useChannels: () => ({ nameOf: (id: string) => `#${id}` }),
}));
vi.mock("@/stores/detail", () => ({ useDetail: () => detail }));
vi.mock("@/lib/thumbnail", () => ({
  thumbnailObjectUrl: vi.fn(async () => "blob:thumb"),
}));
vi.mock("vue-router", () => ({
  useRoute: () => route,
  useRouter: () => router,
}));

function inboxView() {
  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  return mount(InboxView, {
    attachTo: document.body,
    global: { plugins: [i18n] },
  });
}

/** The titles of the rows on screen, in order. */
const titles = (view: ReturnType<typeof inboxView>) =>
  view.findAll("[data-row]").map((element) => element.attributes("data-row"));

function key(name: string, target: EventTarget = window) {
  target.dispatchEvent(
    new KeyboardEvent("keydown", { key: name, bubbles: true }),
  );
}

describe("InboxView", () => {
  let view: ReturnType<typeof inboxView> | undefined;

  beforeEach(() => {
    Object.assign(inbox, {
      filters: { ...emptyFilters(), channels: ["news"] },
      groups: [digest(), group(row("ntf_x"))],
      loading: false,
      error: null,
      pendingCount: 0,
      hasMore: false,
      loadingMore: false,
    });
    setActivePinia(createPinia());
    // jsdom has neither; the theme store and the list's scrolling reach for them.
    vi.stubGlobal(
      "matchMedia",
      vi.fn(() => ({
        matches: false,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      })),
    );
    Element.prototype.scrollIntoView = vi.fn();
    detail.id = null;
    route.params = { channel: "news" };
    vi.clearAllMocks();
  });
  afterEach(() => {
    view?.unmount();
    view = undefined;
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("expands a group to its other members, and collapses it back", async () => {
    view = inboxView();
    expect(titles(view)).toEqual(["ntf_c", "ntf_x"]);
    expect(view.text()).toContain("+2");

    const toggle = view.get("button[aria-expanded]");
    expect(toggle.text()).toContain(
      "Show the 2 other notifications in this group",
    );

    await toggle.trigger("click");
    expect(titles(view)).toEqual(["ntf_c", "ntf_b", "ntf_a", "ntf_x"]);
    expect(toggle.attributes("aria-expanded")).toBe("true");
    expect(toggle.text()).toContain("Collapse the group");

    await view.get('[data-row="ntf_a"]').trigger("click");
    expect(router.push).toHaveBeenCalledWith({
      name: "channel-notification",
      params: { channel: "news", id: "ntf_a" },
    });

    await toggle.trigger("click");
    expect(titles(view)).toEqual(["ntf_c", "ntf_x"]);
  });

  it("counts notifications, not groups, and does not call an open channel a filter", async () => {
    view = inboxView();
    expect(view.text()).toContain("#news");
    expect(view.text()).toContain("4 shown");
    expect(view.text()).not.toContain("Filtered");

    inbox.filters = {
      ...emptyFilters(),
      channels: ["news"],
      status: ["failure"],
    };
    await nextTick();
    expect(view.text()).toContain("Filtered");
  });

  it("walks the rows on screen with the arrow keys, members of an open group included", async () => {
    view = inboxView();
    await view.get("button[aria-expanded]").trigger("click");

    key("ArrowDown");
    key("ArrowDown");
    key("ArrowDown");
    key("ArrowDown");
    key("ArrowDown");
    const walked = router.replace.mock.calls.map(
      ([target]) => target.params.id,
    );
    expect(walked).toEqual(["ntf_b", "ntf_a", "ntf_x", "ntf_x", "ntf_x"]);

    key("ArrowUp");
    expect(router.replace).toHaveBeenLastCalledWith({
      name: "channel-notification",
      params: { channel: "news", id: "ntf_a" },
    });
  });

  it("leaves the keys to a field, and Escape closes the open notification", () => {
    view = inboxView();
    const input = document.createElement("input");
    document.body.append(input);
    key("ArrowDown", input);
    expect(router.replace).not.toHaveBeenCalled();
    input.remove();

    key("Escape");
    expect(router.push).not.toHaveBeenCalled();

    detail.id = "ntf_c";
    key("Escape");
    expect(router.push).toHaveBeenCalledWith({ name: "inbox" });
  });

  it("opens outside any channel from the all-channels inbox", async () => {
    route.params = {};
    inbox.filters = emptyFilters();
    inbox.groups = [];
    view = inboxView();
    expect(view.text()).toContain("All channels");
    key("ArrowDown");
    expect(router.replace).not.toHaveBeenCalled();

    inbox.groups = [group(row("ntf_x"))];
    await nextTick();
    await view.get('[data-row="ntf_x"]').trigger("click");
    expect(router.push).toHaveBeenCalledWith({
      name: "notification",
      params: { id: "ntf_x" },
    });
  });

  it("tells an empty inbox from a filter that matches nothing", async () => {
    inbox.groups = [];
    view = inboxView();
    expect(view.text()).toContain("Nothing to read.");

    inbox.filters = { ...emptyFilters(), minPriority: "high" };
    await nextTick();
    expect(view.text()).toContain("No notification matches these filters.");
  });

  it("shows loading, then a failure that can be retried", async () => {
    inbox.loading = true;
    view = inboxView();
    expect(titles(view)).toEqual([]);

    inbox.loading = false;
    inbox.error = "boom";
    await nextTick();
    expect(view.text()).toContain("boom");
    await view
      .findAll("button")
      .find((b) => b.text() === "Retry")
      ?.trigger("click");
    expect(inbox.load).toHaveBeenCalled();
  });

  it("reveals arrivals behind the separator and loads the next page", async () => {
    inbox.pendingCount = 2;
    inbox.hasMore = true;
    view = inboxView();

    await view
      .findAll("button")
      .find((b) => b.text().includes("2 new notifications"))
      ?.trigger("click");
    expect(inbox.revealPending).toHaveBeenCalled();

    await view
      .findAll("button")
      .find((b) => b.text() === "Load more")
      ?.trigger("click");
    expect(inbox.loadMore).toHaveBeenCalled();
  });

  it("renders what a row carries: priority, revisions, progress, image and lateness", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    // Opened now; the row lands a minute later and is looked at twenty minutes on.
    vi.setSystemTime(openedAt);
    inbox.groups = [];
    view = inboxView();
    vi.setSystemTime(openedAt + 20 * 60_000);
    inbox.groups = [
      group(
        row("ntf_u", {
          lastSeq: 3,
          head: {
            state: "working",
            priority: "urgent",
            received_at: new Date(openedAt + 60_000).toISOString(),
          },
          content: {
            tags: ["a", "b", "c", "d"],
            progress: { current: 2, total: 5 },
            image: { kind: "data_uri", uri: "data:image/png;base64,AA==" },
          },
        }),
      ),
      group(row("ntf_h", { head: { priority: "high" } })),
    ];
    detail.id = "ntf_u";
    await nextTick();

    const urgent = view.get('[data-row="ntf_u"]');
    expect(urgent.attributes("aria-current")).toBe("true");
    expect(urgent.text()).toContain("3 revisions");
    expect(urgent.text()).toContain("2/5");
    expect(urgent.text()).toContain("received late");
    expect(urgent.find("img").exists()).toBe(true);
    expect(view.get('[data-row="ntf_h"]').text()).not.toContain(
      "received late",
    );
  });
});
