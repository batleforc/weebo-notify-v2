/** The channel list navigates (URL, back button, other pages) rather than toggling a filter. */
import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createI18n } from "vue-i18n";
import { createMemoryHistory, createRouter } from "vue-router";

import en from "@/locales/en.json";
import AppSidebar from "./AppSidebar.vue";

vi.mock("@/api/generated", () => ({
  inbox: vi.fn(async () => ({ data: { items: [], next: null } })),
  channels: vi.fn(async () => ({ data: { items: [] } })),
  unreadCounts: vi.fn(async () => ({ data: { items: [] } })),
}));
vi.mock("@/api/stream", () => ({ openStream: () => ({ close: () => {} }) }));

const CHANNELS = [
  { id: "ci-infra", display_name: "CI infra", origin: "managed" },
  { id: "cluster-ovh-1", display_name: "Cluster OVH 1", origin: "managed" },
];

/** A stand-in for every route that draws this sidebar. */
const routes = [
  { path: "/", name: "inbox", component: { template: "<div />" } },
  { path: "/c/:channel", name: "channel", component: { template: "<div />" } },
  {
    path: "/c/:channel/n/:id",
    name: "channel-notification",
    component: { template: "<div />" },
  },
  { path: "/n/:id", name: "notification", component: { template: "<div />" } },
  { path: "/settings", name: "settings", component: { template: "<div />" } },
  { path: "/admin", name: "admin", component: { template: "<div />" } },
  { path: "/console", name: "console", component: { template: "<div />" } },
];

async function sidebar(at = "/") {
  const router = createRouter({ history: createMemoryHistory(), routes });
  await router.push(at);
  await router.isReady();

  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  const wrapper = mount(AppSidebar, {
    global: {
      plugins: [router, i18n],
      stubs: { Tooltip: true, Collapsible: true, Skeleton: true },
    },
  });

  // The list is drawn from the store; seeding it directly keeps this about the
  // sidebar's behaviour rather than about how channels are fetched.
  const { useChannels } = await import("@/stores/channels");
  const store = useChannels();
  store.all = CHANNELS as never;
  store.loading = false;
  await wrapper.vm.$nextTick();

  return { wrapper, router };
}

/** Click, then let the router settle. */
async function click(
  wrapper: Awaited<ReturnType<typeof sidebar>>["wrapper"],
  label: string,
): Promise<void> {
  await button(wrapper, label).trigger("click");
  await flushPromises();
}

/** The button whose visible label is `label`. */
function button(
  wrapper: Awaited<ReturnType<typeof sidebar>>["wrapper"],
  label: string,
) {
  const found = wrapper
    .findAll("button")
    .find((candidate) => candidate.text().includes(label));
  if (!found) {
    throw new Error(`no button labelled ${label}`);
  }
  return found;
}

describe("the sidebar's channel list", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    // jsdom has no `matchMedia`; the theme store needs one.
    vi.stubGlobal(
      "matchMedia",
      vi.fn(() => ({
        matches: false,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      })),
    );
  });

  it("navigates to a channel rather than ticking it", async () => {
    const { wrapper, router } = await sidebar("/");

    await click(wrapper, "CI infra");

    expect(router.currentRoute.value.name).toBe("channel");
    expect(router.currentRoute.value.params.channel).toBe("ci-infra");
  });

  /** The report that started this: the sidebar is drawn on Settings too. */
  it("leaves Settings for the channel, instead of changing a filter nobody can see", async () => {
    const { wrapper, router } = await sidebar("/settings");

    await click(wrapper, "Cluster OVH 1");

    expect(router.currentRoute.value.path).toBe("/c/cluster-ovh-1");
  });

  it("goes back to everything when the open channel is clicked again", async () => {
    const { wrapper, router } = await sidebar("/c/ci-infra");

    await click(wrapper, "CI infra");

    expect(router.currentRoute.value.name).toBe("inbox");
  });

  it("offers a way out that does not depend on remembering which one is open", async () => {
    const { wrapper, router } = await sidebar("/c/ci-infra");

    await click(wrapper, "All channels");

    expect(router.currentRoute.value.name).toBe("inbox");
  });

  /** The highlight comes from the URL, however it was reached. */
  it("marks the open channel from the route", async () => {
    const { wrapper } = await sidebar("/c/cluster-ovh-1");

    expect(button(wrapper, "Cluster OVH 1").attributes("aria-current")).toBe(
      "page",
    );
    expect(
      button(wrapper, "CI infra").attributes("aria-current"),
    ).toBeUndefined();
    expect(
      button(wrapper, "All channels").attributes("aria-current"),
    ).toBeUndefined();
  });

  it("marks 'all channels' when no channel is open", async () => {
    const { wrapper } = await sidebar("/");

    expect(button(wrapper, "All channels").attributes("aria-current")).toBe(
      "page",
    );
  });

  /** The nested notification route keeps the channel highlighted. */
  it("stays on the channel while one of its notifications is open", async () => {
    const { wrapper } = await sidebar("/c/ci-infra/n/ntf_1");

    expect(button(wrapper, "CI infra").attributes("aria-current")).toBe("page");
    expect(
      button(wrapper, "All channels").attributes("aria-current"),
    ).toBeUndefined();
  });

  /** A notification reached by bare permalink belongs to no channel. */
  it("marks 'all channels' for a context-free notification permalink", async () => {
    const { wrapper } = await sidebar("/n/ntf_1");

    expect(button(wrapper, "All channels").attributes("aria-current")).toBe(
      "page",
    );
  });
});
