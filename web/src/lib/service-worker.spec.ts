import { beforeEach, describe, expect, it, vi } from "vitest";

const push = vi.fn(() => Promise.resolve());
vi.mock("@/router", () => ({ router: { push } }));

const { onWorkerMessage } = await import("./service-worker");

describe("onWorkerMessage", () => {
  beforeEach(() => push.mockClear());

  it("follows a notification click to the path the worker sent", () => {
    onWorkerMessage({ type: "navigate", url: "/n/ntf_01J" });
    expect(push).toHaveBeenCalledWith("/n/ntf_01J");
  });

  it("refuses anything that is not an in-app path", () => {
    onWorkerMessage({ type: "navigate", url: "https://evil.example/" });
    onWorkerMessage({ type: "navigate", url: 42 });
    onWorkerMessage({ type: "navigate" });
    onWorkerMessage("navigate");
    onWorkerMessage(null);
    expect(push).not.toHaveBeenCalled();
  });
});
