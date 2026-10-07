/** Device enrolment (§6.2): base64url decoding, and permission asked only after availability. */

import { beforeEach, describe, expect, it, vi } from "vitest";

const enrolDevice = vi.fn();
const revokeDevice = vi.fn();
const webpushKey = vi.fn();

vi.mock("@/api/generated", () => ({
  enrolDevice: (...args: unknown[]) => enrolDevice(...args),
  revokeDevice: (...args: unknown[]) => revokeDevice(...args),
  webpushKey: (...args: unknown[]) => webpushKey(...args),
}));

const ready = vi.fn();
vi.mock("@/lib/service-worker", () => ({
  ready: () => ready(),
  register: vi.fn(),
  supported: () => true,
}));

/** The 65-octet uncompressed point a server hands out, base64url and unpadded. */
const KEY =
  "BMGre7JnmZhpSd1wfd8QBHYtNNuMqlA5mPDbzs14tq03pclJZ54o2c1_WCIZWP-gixwzu4Ods98Yb_UHS25OC5w";

const subscription = {
  toJSON: () => ({
    endpoint: "https://push.example/abc",
    keys: { p256dh: "BPk", auth: "k9" },
  }),
  unsubscribe: vi.fn().mockResolvedValue(true),
};

const pushManager = {
  getSubscription: vi.fn().mockResolvedValue(null),
  subscribe: vi.fn().mockResolvedValue(subscription),
};

describe("decoding the application server key", () => {
  it("reads the unpadded base64url RFC 8292 uses", async () => {
    const { decodeKey } = await import("@/lib/push");

    const bytes = decodeKey(KEY);

    expect(bytes).toHaveLength(65);
    expect(bytes[0]).toBe(0x04); // an uncompressed point says so in its first byte
  });

  it("handles both substitutions and every padding length", async () => {
    const { decodeKey } = await import("@/lib/push");

    // `-` and `_` are the two characters standard base64 spells `+` and `/`;
    // getting either wrong throws inside `atob` rather than here.
    expect([...decodeKey("-_8")]).toEqual([0xfb, 0xff]);
    expect([...decodeKey("YQ")]).toEqual([0x61]);
    expect([...decodeKey("YWI")]).toEqual([0x61, 0x62]);
    expect([...decodeKey("YWJj")]).toEqual([0x61, 0x62, 0x63]);
  });
});

describe("enabling push", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    localStorage.clear();

    ready.mockResolvedValue({ pushManager });
    pushManager.getSubscription.mockResolvedValue(null);
    pushManager.subscribe.mockResolvedValue(subscription);
    enrolDevice.mockResolvedValue({ data: { id: "dev_01J" } });
    webpushKey.mockResolvedValue({ data: { application_server_key: KEY } });

    vi.stubGlobal("Notification", {
      permission: "default",
      requestPermission: vi.fn().mockResolvedValue("granted"),
    });
    vi.stubGlobal("PushManager", class {});
    vi.stubGlobal("navigator", {
      serviceWorker: {},
      userAgent: "Mozilla/5.0 (X11; Linux x86_64) Chrome/140.0 Safari/537.36",
    });
  });

  it("subscribes and enrols the subscription as a device", async () => {
    const { enable, enrolledDeviceId } = await import("@/lib/push");

    await enable();

    expect(pushManager.subscribe).toHaveBeenCalledWith(
      expect.objectContaining({ userVisibleOnly: true }),
    );
    expect(enrolDevice).toHaveBeenCalledWith(
      expect.objectContaining({
        body: expect.objectContaining({
          transport: {
            transport: "web_push",
            endpoint: "https://push.example/abc",
            p256dh: "BPk",
            auth: "k9",
          },
        }),
      }),
    );
    // Remembered locally, because nothing else can tell two browser profiles apart.
    expect(enrolledDeviceId()).toBe("dev_01J");
  });

  it("never prompts on a deployment that runs no Web Push", async () => {
    webpushKey.mockResolvedValue({ data: undefined });
    const { enable, PushRefused } = await import("@/lib/push");

    await expect(enable()).rejects.toBeInstanceOf(PushRefused);

    // The order is the point: asking for a permission that could not be honoured
    // spends the one prompt a browser gives, and it is not given back.
    expect(Notification.requestPermission).not.toHaveBeenCalled();
    expect(pushManager.subscribe).not.toHaveBeenCalled();
  });

  it("does not enrol a second device when a subscription already exists", async () => {
    pushManager.getSubscription.mockResolvedValue(subscription);
    const { enable } = await import("@/lib/push");

    await enable();

    // `subscribe()` with the same key returns the same endpoint, so re-subscribing
    // would produce a second device for one browser.
    expect(pushManager.subscribe).not.toHaveBeenCalled();
    expect(enrolDevice).toHaveBeenCalledTimes(1);
  });

  it("says so rather than throwing when permission is refused", async () => {
    vi.stubGlobal("Notification", {
      permission: "default",
      requestPermission: vi.fn().mockResolvedValue("denied"),
    });
    const { enable, PushRefused } = await import("@/lib/push");

    await expect(enable()).rejects.toMatchObject({ reason: "denied" });
    await expect(enable()).rejects.toBeInstanceOf(PushRefused);
    expect(enrolDevice).not.toHaveBeenCalled();
  });

  it("drops the subscription before revoking the device", async () => {
    pushManager.getSubscription.mockResolvedValue(subscription);
    revokeDevice.mockResolvedValue({});
    const { enable, disable, enrolledDeviceId } = await import("@/lib/push");
    await enable();

    await disable();

    // That order, and not the other: a revoked device with a live subscription is
    // a push the server sends and nothing routes.
    expect(subscription.unsubscribe).toHaveBeenCalled();
    expect(revokeDevice).toHaveBeenCalledWith({ path: { id: "dev_01J" } });
    expect(enrolledDeviceId()).toBeNull();
  });
});
