/**
 * Web Push enrolment (§6.2, §8): fetch the server's VAPID key (never baked into
 * the bundle; 501 = no Web Push), ask permission, subscribe, enrol as a device.
 * The device id lives in `localStorage`, one per browser profile.
 */

import { enrolDevice, revokeDevice, webpushKey } from "@/api/generated";
import { i18n } from "@/i18n";
import { isDesktop } from "@/lib/desktop";
import { ready } from "@/lib/service-worker";

const DEVICE_KEY = "weebo-notify:device";

/** Last signed-in `sub`: the device and subscription are the browser's, not a person's. */
const OWNER_KEY = "weebo-notify:device-owner";

export interface PushState {
  /** Whether this browser can do Web Push at all. */
  supported: boolean;
  /** Whether the deployment runs a Web Push transport. */
  available: boolean;
  permission: NotificationPermission;
  /** Whether this browser is enrolled and subscribed right now. */
  enabled: boolean;
}

/** Never in the desktop shell: it notifies natively and has no service worker. */
export function supported(): boolean {
  return (
    !isDesktop() &&
    "serviceWorker" in navigator &&
    "PushManager" in window &&
    "Notification" in window
  );
}

export function enrolledDeviceId(): string | null {
  return localStorage.getItem(DEVICE_KEY);
}

/** Side-effect free: called on every Settings render, so it must never prompt. */
export async function state(): Promise<PushState> {
  if (!supported()) {
    return {
      supported: false,
      available: false,
      permission: "denied",
      enabled: false,
    };
  }

  const available = (await applicationServerKey()) !== null;
  const registration = await ready();
  const subscription =
    (await registration?.pushManager.getSubscription()) ?? null;

  return {
    supported: true,
    available,
    permission: Notification.permission,
    // Needs both: a subscription and an enrolled device.
    enabled: subscription !== null && enrolledDeviceId() !== null,
  };
}

/** Cached per page: a key rotation is a reload anyway. */
let cachedKey: string | null | undefined;

export async function applicationServerKey(): Promise<string | null> {
  if (cachedKey !== undefined) {
    return cachedKey;
  }

  const answer = await webpushKey();
  // 501 (§14.3) or any other failure: the switch stays off.
  cachedKey = answer.data?.application_server_key ?? null;
  return cachedKey;
}

export class PushRefused extends Error {
  constructor(
    readonly reason: "unsupported" | "unavailable" | "denied" | "failed",
    message: string,
  ) {
    super(message);
    this.name = "PushRefused";
  }
}

/** Reuses an existing subscription: re-enrolling would create a second device. */
export async function enable(label?: string): Promise<void> {
  if (!supported()) {
    throw new PushRefused(
      "unsupported",
      i18n.global.t("push.refused.unsupported"),
    );
  }

  const key = await applicationServerKey();
  if (!key) {
    throw new PushRefused(
      "unavailable",
      i18n.global.t("push.refused.unavailable"),
    );
  }

  const permission = await Notification.requestPermission();
  if (permission !== "granted") {
    throw new PushRefused("denied", i18n.global.t("push.refused.denied"));
  }

  const registration = await ready();
  if (!registration) {
    throw new PushRefused("failed", i18n.global.t("push.refused.failed"));
  }

  const existing = await registration.pushManager.getSubscription();
  const subscription =
    existing ??
    (await registration.pushManager.subscribe({
      // Required: a browser that sees an unseen push revokes the subscription.
      userVisibleOnly: true,
      applicationServerKey: decodeKey(key),
    }));

  await enrol(subscription, label);
}

/** Unsubscribe before revoking: the harmless order. */
export async function disable(): Promise<void> {
  const registration = await ready();
  const subscription = await registration?.pushManager.getSubscription();
  await subscription?.unsubscribe();

  const id = enrolledDeviceId();
  if (id) {
    localStorage.removeItem(DEVICE_KEY);
    await revokeDevice({ path: { id } }).catch(() => undefined);
  }
}

/**
 * Local half of sign-out, no API: unsubscribe and drop the device id (the push
 * service's `410` then revokes it). No-op where push is unsupported.
 */
export async function dropLocalPush(): Promise<void> {
  localStorage.removeItem(DEVICE_KEY);
  if (!supported()) {
    return;
  }
  const registration = await ready();
  const subscription = await registration?.pushManager.getSubscription();
  await subscription?.unsubscribe();
}

/** Record who is signed in; drop push if it was someone else's. An unknown owner is adopted. */
export async function claimFor(subject: string): Promise<void> {
  const owner = localStorage.getItem(OWNER_KEY);
  localStorage.setItem(OWNER_KEY, subject);
  if (owner !== null && owner !== subject) {
    await dropLocalPush();
  }
}

/** Re-enrol after a subscription rotation, revoking the stale device. */
export async function resubscribe(): Promise<void> {
  const registration = await ready();
  const key = await applicationServerKey();
  if (!registration || !key) {
    return;
  }

  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: decodeKey(key),
  });

  const stale = enrolledDeviceId();
  if (stale) {
    await revokeDevice({ path: { id: stale } }).catch(() => undefined);
  }
  await enrol(subscription);
}

async function enrol(
  subscription: PushSubscription,
  label?: string,
): Promise<void> {
  const json = subscription.toJSON();
  const keys = json.keys ?? {};
  if (!json.endpoint || !keys.p256dh || !keys.auth) {
    throw new PushRefused("failed", i18n.global.t("push.refused.incomplete"));
  }

  const answer = await enrolDevice({
    body: {
      label: label ?? browserLabel(),
      transport: {
        transport: "web_push",
        endpoint: json.endpoint,
        p256dh: keys.p256dh,
        auth: keys.auth,
      },
    },
    throwOnError: true,
  });

  localStorage.setItem(DEVICE_KEY, answer.data.id);
}

/** A label parsed from the UA, never an identity. */
function browserLabel(): string {
  const agent = navigator.userAgent;
  const browser = /Firefox\/\d/.test(agent)
    ? "Firefox"
    : /Edg\/\d/.test(agent)
      ? "Edge"
      : /Chrome\/\d/.test(agent)
        ? "Chrome"
        : /Safari\/\d/.test(agent)
          ? "Safari"
          : i18n.global.t("push.label.browser");

  const platform = /Android/.test(agent)
    ? "Android"
    : /iPhone|iPad/.test(agent)
      ? "iOS"
      : /Mac OS X/.test(agent)
        ? "macOS"
        : /Windows/.test(agent)
          ? "Windows"
          : /Linux/.test(agent)
            ? "Linux"
            : null;

  return platform
    ? i18n.global.t("push.label.onPlatform", { browser, platform })
    : browser;
}

/** Unpadded base64url (RFC 8292) to bytes; `atob` needs standard padded base64. */
export function decodeKey(base64url: string): Uint8Array<ArrayBuffer> {
  const padded = base64url
    .replace(/-/g, "+")
    .replace(/_/g, "/")
    .padEnd(base64url.length + ((4 - (base64url.length % 4)) % 4), "=");

  const binary = atob(padded);
  // Explicit ArrayBuffer: `BufferSource` excludes SharedArrayBuffer.
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
