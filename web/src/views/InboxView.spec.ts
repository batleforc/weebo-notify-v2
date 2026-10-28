/** The inbox list (§10.4): a group's members stay reachable, and the open channel is no filter. */
import { mount } from "@vue/test-utils";
import { describe, expect, it, vi } from "vitest";
import { reactive } from "vue";
import { createI18n } from "vue-i18n";

import en from "@/locales/en.json";
import { emptyFilters, type Row } from "@/stores/inbox";
import InboxView from "./InboxView.vue";

function row(id: string, groupKey?: string): Row {
  return {
    id,
    channel: "news",
    head: { content: { title: id, group_key: groupKey } },
  } as unknown as Row;
}

const head = row("ntf_c", "digest");
const inbox = reactive({
  filters: { ...emptyFilters(), channels: ["news"] },
  groups: [
    { head, members: [head, row("ntf_b", "digest"), row("ntf_a", "digest")] },
    { head: row("ntf_x"), members: [row("ntf_x")] },
  ],
  loading: false,
  error: null,
  pendingCount: 0,
  hasMore: false,
});

vi.mock("@/stores/inbox", async (original) => ({
  ...(await original<typeof import("@/stores/inbox")>()),
  useInbox: () => inbox,
}));
vi.mock("@/stores/channels", () => ({
  useChannels: () => ({ nameOf: (id: string) => id }),
}));
vi.mock("@/stores/detail", () => ({ useDetail: () => ({ id: null }) }));
vi.mock("vue-router", () => ({
  useRoute: () => ({ params: { channel: "news" } }),
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
}));

function inboxView() {
  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  return mount(InboxView, {
    global: {
      plugins: [i18n],
      stubs: {
        NotificationRow: {
          props: ["row"],
          template: '<button type="button">{{ row.id }}</button>',
        },
      },
    },
  });
}

const titles = (view: ReturnType<typeof inboxView>) =>
  view.findAll("li button:not([aria-expanded])").map((button) => button.text());

describe("InboxView", () => {
  it("expands a group to its other members, and collapses it back", async () => {
    const view = inboxView();
    expect(titles(view)).toEqual(["ntf_c", "ntf_x"]);

    const toggle = view.get("button[aria-expanded]");
    expect(toggle.text()).toContain(
      "Show the 2 other notifications in this group",
    );

    await toggle.trigger("click");
    expect(titles(view)).toEqual(["ntf_c", "ntf_b", "ntf_a", "ntf_x"]);
    expect(toggle.attributes("aria-expanded")).toBe("true");

    await toggle.trigger("click");
    expect(titles(view)).toEqual(["ntf_c", "ntf_x"]);
  });

  it("counts notifications, not groups, and does not call an open channel a filter", () => {
    const view = inboxView();
    expect(view.text()).toContain("4 shown");
    expect(view.text()).not.toContain("Filtered");
  });
});
