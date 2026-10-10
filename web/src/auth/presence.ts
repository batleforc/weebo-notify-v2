/**
 * "Is any other tab of this browser still signed in?"
 *
 * The session is per tab (`sessionStorage`, ADR-0020) but the push subscription
 * and the enrolled device are per browser (`lib/push`). When a tab's session dies
 * without a sign-out — a refresh refused with `invalid_grant` — the subscription
 * must outlive it only if some other tab still has a session riding on it.
 * Otherwise nothing is left to revoke it, and notifications keep landing on a
 * lock screen nobody is signed in behind.
 *
 * Asked over a `BroadcastChannel` (ping, any signed-in tab pongs). Where that does
 * not exist, signed-in tabs keep a timestamp fresh in `localStorage` instead.
 */

const CHANNEL = "weebo-notify:presence";
const HEARTBEAT_PREFIX = "weebo-notify:presence:";
const HEARTBEAT_MS = 5_000;
/**
 * How long to wait for a pong. Generous for a same-process message hop, short
 * enough that a sign-out-by-expiry is not visibly slower.
 */
const ANSWER_TIMEOUT_MS = 300;

const channel: BroadcastChannel | null =
  typeof BroadcastChannel === "undefined"
    ? null
    : new BroadcastChannel(CHANNEL);

/** Waiting `anotherTabSignedIn` calls, resolved by the first pong. */
const waiting = new Set<() => void>();

/** Only for the heartbeat fallback: a key of this tab's own. */
const tab = Math.random().toString(36).slice(2);

/** Start answering for this tab. `signedIn` is read at the moment of asking. */
export function announcePresence(signedIn: () => boolean): void {
  if (channel) {
    // One channel both answers and asks, so a tab never hears its own ping.
    channel.onmessage = (event: MessageEvent<"ping" | "pong">) => {
      if (event.data === "ping" && signedIn()) {
        channel.postMessage("pong");
      } else if (event.data === "pong") {
        for (const resolve of waiting) resolve();
      }
    };
    return;
  }

  const key = HEARTBEAT_PREFIX + tab;
  const beat = () => {
    if (signedIn()) {
      localStorage.setItem(key, String(Date.now()));
    } else {
      localStorage.removeItem(key);
    }
  };
  beat();
  setInterval(beat, HEARTBEAT_MS);
  window.addEventListener("pagehide", () => localStorage.removeItem(key));
}

/**
 * Whether another tab answers as signed in. Never rejects. Asked only by a tab
 * whose own session just ended.
 */
export function anotherTabSignedIn(): Promise<boolean> {
  if (!channel) {
    // This tab is no longer signed in; without this its beat would still count for
    // the next tab to ask, and the last of two dying tabs would keep push alive.
    localStorage.removeItem(HEARTBEAT_PREFIX + tab);
    // Three missed beats: a tab that crashed stops counting in 15 s rather than
    // keeping push alive for good.
    const freshAfter = Date.now() - 3 * HEARTBEAT_MS;
    for (let i = 0; i < localStorage.length; i += 1) {
      const key = localStorage.key(i);
      if (
        key?.startsWith(HEARTBEAT_PREFIX) &&
        key !== HEARTBEAT_PREFIX + tab &&
        Number(localStorage.getItem(key)) > freshAfter
      ) {
        return Promise.resolve(true);
      }
    }
    return Promise.resolve(false);
  }

  const asking = channel;
  return new Promise((resolve) => {
    const answered = () => settle(true);
    const timer = setTimeout(() => settle(false), ANSWER_TIMEOUT_MS);
    function settle(value: boolean) {
      clearTimeout(timer);
      waiting.delete(answered);
      resolve(value);
    }
    waiting.add(answered);
    asking.postMessage("ping");
  });
}
