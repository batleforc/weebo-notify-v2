/**
 * Turning a browser into a device the server can reach (design §6.2, §8, M3.6).
 *
 * Four steps, in an order that is not free:
 *
 *   1. ask the server which key it identifies itself with (`GET /push/webpush`);
 *   2. ask the person for permission — after step 1, so a deployment that runs no
 *      Web Push never raises a prompt it cannot honour;
 *   3. subscribe with the push service, through the service worker;
 *   4. enrol the subscription as a device (`POST /devices`).
 *
 * Step 4 is what makes it a *device* rather than a browser tab: revoking it kills
 * the push subscription and the sync cursor together (§15), which is the whole
 * point of devices being first-class.
 *
 * # The key comes from the server, never from the bundle
 *
 * RFC 8292 makes the server's public key an argument of `subscribe()`. Baking it
 * into the build was the obvious shortcut and it is wrong: rotating a VAPID key
 * would need a rebuilt frontend to keep push working, and ADR-0009's rule that a
 * client is derived from the contract would be broken by the one value that
 * decides whether anything arrives. A `501` here means this deployment runs no
 * Web Push, and the UI says so instead of retrying.
 *
 * # One device per browser profile, remembered locally
 *
 * The device id lives in `localStorage`, because nothing else can tell two
 * browsers apart: the same person on the same machine in Chrome and Firefox is two
 * subscriptions, two endpoints, and rightly two devices. Losing the id (cleared
 * storage, a new profile) enrols a second one — visible and revocable in Settings,
 * which is better than the alternative of guessing they are the same and revoking
 * something else's push.
 */

import { enrolDevice, revokeDevice, webpushKey } from "@/api/generated";
import { i18n } from "@/i18n";
import { ready } from "@/lib/service-worker";

/** Where the enrolled device id is remembered, per browser profile. */
const DEVICE_KEY = "weebo-notify:device";

/** What the UI needs to know to draw one switch honestly. */
export interface PushState {
  /** Whether this browser can do Web Push at all. */
  supported: boolean;
  /** Whether the deployment runs a Web Push transport. */
  available: boolean;
  /** The browser's notification permission, as it stands. */
  permission: NotificationPermission;
  /** Whether this browser is enrolled and subscribed right now. */
  enabled: boolean;
}

/** Whether the three APIs this needs exist. */
export function supported(): boolean {
  return (
    "serviceWorker" in navigator &&
    "PushManager" in window &&
    "Notification" in window
  );
}

/** The device id this browser enrolled under, if it has one. */
export function enrolledDeviceId(): string | null {
  return localStorage.getItem(DEVICE_KEY);
}

/**
 * Where things stand, without changing anything.
 *
 * Deliberately side-effect free — it is called on every render of the Settings
 * panel, and a status read that asks for permission would be a trap.
 */
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
    // Both halves have to hold: a subscription with no enrolled device sends
    // pushes nobody routes, and an enrolled device with no subscription is a row
    // in Settings that will never ring.
    enabled: subscription !== null && enrolledDeviceId() !== null,
  };
}

/**
 * The `applicationServerKey`, or `null` on a deployment without Web Push.
 *
 * Cached for the lifetime of the page: it is a constant of the deployment, and
 * asking again on every toggle would be a round trip for an answer that cannot
 * have changed. A rotation is a reload, which is also when every subscription
 * taken out under the old key stopped working anyway.
 */
let cachedKey: string | null | undefined;

export async function applicationServerKey(): Promise<string | null> {
  if (cachedKey !== undefined) {
    return cachedKey;
  }

  const answer = await webpushKey();
  // `501` is the documented "this deployment does not do that" (§14.3), and any
  // other failure is treated the same way here: the switch stays off and says
  // why, rather than throwing inside a settings page.
  cachedKey = answer.data?.application_server_key ?? null;
  return cachedKey;
}

/** Why enabling push did not work, in the words the UI shows. */
export class PushRefused extends Error {
  constructor(
    readonly reason: "unsupported" | "unavailable" | "denied" | "failed",
    message: string,
  ) {
    super(message);
    this.name = "PushRefused";
  }
}

/**
 * Subscribe this browser and enrol it as a device.
 *
 * Idempotent in the way that matters: an existing subscription is reused rather
 * than replaced, because `subscribe()` with the same key returns the same
 * endpoint and re-enrolling it would produce a second device for one browser.
 */
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

  // After the availability check, so nobody is asked for a permission that
  // could not be used, and before subscribing, which needs it granted.
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
      // Promising a visible notification for every push is what keeps the
      // subscription alive: a browser that catches one going unseen revokes it.
      userVisibleOnly: true,
      applicationServerKey: decodeKey(key),
    }));

  await enrol(subscription, label);
}

/**
 * Stop this browser receiving push.
 *
 * The subscription is dropped first and the device revoked second, in that order:
 * a revoked device with a live subscription is a push the server will send and
 * nothing will route, while a dropped subscription with a live device only costs
 * the row until the next call — the harmless way round.
 */
export async function disable(): Promise<void> {
  const registration = await ready();
  const subscription = await registration?.pushManager.getSubscription();
  await subscription?.unsubscribe();

  const id = enrolledDeviceId();
  if (id) {
    localStorage.removeItem(DEVICE_KEY);
    // A device already gone server-side (revoked from another client, expired
    // relay) is the same outcome as one revoked here.
    await revokeDevice({ path: { id } }).catch(() => undefined);
  }
}

/**
 * Re-enrol after the push service rotated the subscription.
 *
 * The old endpoint stopped working the moment it rotated, so the stale device is
 * revoked rather than left behind — otherwise Settings fills up with rows that
 * cannot ring, one per rotation.
 */
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

/** Hand a subscription to the API and remember what came back. */
async function enrol(
  subscription: PushSubscription,
  label?: string,
): Promise<void> {
  const json = subscription.toJSON();
  const keys = json.keys ?? {};
  if (!json.endpoint || !keys.p256dh || !keys.auth) {
    throw new PushRefused(
      "failed",
      "Le navigateur a rendu un abonnement incomplet.",
    );
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

/**
 * A name somebody will recognise in a device list.
 *
 * Parsed from the user agent, which is a poor source and the only one a browser
 * offers. It is a *label*, never an identity — nothing is decided from it — so
 * being wrong costs a confusing row and no more, and it stays editable server-side.
 */
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
          : "Navigateur";

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

  return platform ? `${browser} sur ${platform}` : browser;
}

/**
 * base64url to the bytes `subscribe()` wants.
 *
 * `applicationServerKey` takes a `BufferSource`, and the server sends the
 * unpadded base64url of RFC 8292 — which `atob` does not read: it wants standard
 * base64, padded. Both substitutions are needed, and forgetting either produces
 * an `InvalidCharacterError` several stack frames from here.
 */
export function decodeKey(base64url: string): Uint8Array<ArrayBuffer> {
  const padded = base64url
    .replace(/-/g, "+")
    .replace(/_/g, "/")
    .padEnd(base64url.length + ((4 - (base64url.length % 4)) % 4), "=");

  const binary = atob(padded);
  // The buffer is allocated rather than left to the length constructor so the
  // type is `Uint8Array<ArrayBuffer>` and not `<ArrayBufferLike>`: `BufferSource`
  // excludes a `SharedArrayBuffer`, and `subscribe()` would refuse one.
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
