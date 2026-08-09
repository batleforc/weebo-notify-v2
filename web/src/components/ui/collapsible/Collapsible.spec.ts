import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";

import { Collapsible } from ".";

function collapsible(open?: boolean) {
  return mount(Collapsible, {
    props: open === undefined ? {} : { open },
    slots: {
      default: "<button>toggle</button>",
      content: "<p>body</p>",
    },
  });
}

describe("Collapsible", () => {
  it("starts closed by default", () => {
    const wrapper = collapsible();
    expect(wrapper.find("button").attributes("aria-expanded")).toBe("false");
  });

  it("opens when the trigger is clicked", async () => {
    const wrapper = collapsible();
    await wrapper.find("button").trigger("click");
    expect(wrapper.find("button").attributes("aria-expanded")).toBe("true");
    expect(wrapper.text()).toContain("body");
  });

  it("closes again on a second click", async () => {
    const wrapper = collapsible();
    await wrapper.find("button").trigger("click");
    await wrapper.find("button").trigger("click");
    expect(wrapper.find("button").attributes("aria-expanded")).toBe("false");
  });

  it("respects the open model prop", () => {
    const wrapper = collapsible(true);
    expect(wrapper.find("button").attributes("aria-expanded")).toBe("true");
    expect(wrapper.text()).toContain("body");
  });

  it("emits update:open when toggled", async () => {
    const wrapper = collapsible(false);
    await wrapper.find("button").trigger("click");
    expect(wrapper.emitted("update:open")).toEqual([[true]]);
  });
});
