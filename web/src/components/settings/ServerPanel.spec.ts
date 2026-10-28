/** RFC 0001: "Change server" only where the server was chosen, not baked in. */

import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createI18n } from "vue-i18n";

import en from "@/locales/en.json";

const { preset } = vi.hoisted(() => ({ preset: { value: false } }));
vi.mock("@/lib/instance", () => ({
  isPresetInstance: () => preset.value,
  forgetInstance: vi.fn(),
}));
vi.mock("@/config", () => ({ apiBaseUrl: "https://notify.example.org" }));
vi.mock("@/auth/session", () => ({ useSession: () => ({ logout: vi.fn() }) }));

import ServerPanel from "./ServerPanel.vue";

function render() {
  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  return mount(ServerPanel, { global: { plugins: [i18n] } });
}

beforeEach(() => {
  setActivePinia(createPinia());
});

describe("the server panel", () => {
  it("offers to change a server the person chose", () => {
    preset.value = false;
    const panel = render();
    expect(panel.text()).toContain("https://notify.example.org");
    expect(panel.find("button").text()).toBe("Change server");
  });

  it("says a server fixed by the build cannot be changed, with no button", () => {
    preset.value = true;
    const panel = render();
    expect(panel.find("button").exists()).toBe(false);
    expect(panel.text()).toContain("cannot be changed here");
  });
});
