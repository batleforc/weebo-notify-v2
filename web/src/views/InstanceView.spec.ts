/** The first-launch screen (RFC 0001): only a confirmed instance reloads the app. */
import { flushPromises, mount } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createI18n } from "vue-i18n";

import en from "@/locales/en.json";
import InstanceView from "./InstanceView.vue";

const { probeInstance, confirmInstance } = vi.hoisted(() => ({
  probeInstance: vi.fn(),
  confirmInstance: vi.fn(),
}));
vi.mock("@/lib/instance", () => ({ probeInstance, confirmInstance }));

const reload = vi.fn();

function instanceView() {
  const i18n = createI18n({ legacy: false, locale: "en", messages: { en } });
  return mount(InstanceView, { global: { plugins: [i18n] } });
}

async function submit(address: string) {
  const view = instanceView();
  await view.find("input").setValue(address);
  await view.find("form").trigger("submit");
  await flushPromises();
  return view;
}

describe("InstanceView", () => {
  beforeEach(() => {
    probeInstance.mockReset();
    confirmInstance.mockReset();
    reload.mockReset();
    vi.stubGlobal("location", { reload });
  });
  afterEach(() => vi.unstubAllGlobals());

  it("reloads once the person confirms the server the shell checked", async () => {
    probeInstance.mockResolvedValue({
      probe: "7",
      api: "https://notify.example.org/",
      issuerHost: "auth.example.org",
    });
    confirmInstance.mockResolvedValue(true);

    await submit("https://notify.example.org");

    expect(probeInstance).toHaveBeenCalledWith("https://notify.example.org");
    expect(confirmInstance).toHaveBeenCalledWith("7");
    expect(reload).toHaveBeenCalled();
  });

  it("stores nothing and says so when the dialog is declined", async () => {
    probeInstance.mockResolvedValue({ probe: "1", api: "x", issuerHost: "y" });
    confirmInstance.mockResolvedValue(false);

    const view = await submit("https://notify.example.org");

    expect(reload).not.toHaveBeenCalled();
    expect(view.find("[role=alert]").text()).toContain("you chose not to use");
  });

  it("shows why the shell refused the address, and never asks to confirm it", async () => {
    probeInstance.mockRejectedValue("only https addresses are accepted");

    const view = await submit("http://notify.example.org");

    expect(confirmInstance).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
    expect(view.find("[role=alert]").text()).toContain(
      "only https addresses are accepted",
    );
  });
});
