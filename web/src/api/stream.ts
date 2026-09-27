/**
 * The live stream (design §8.1), read with `fetch` rather than `EventSource`.
 *
 * # Why not `EventSource`
 *
 * `EventSource` cannot send an `Authorization` header. Every workaround for that
 * is worse than reading the body ourselves: a token in the query string ends up in
 * access logs and browser history, and a cookie would hand the API the CSRF
 * surface §5.1 exists to avoid. So the stream is a `fetch` whose body is decoded
 * here, which also means:
 *
 *   * `Last-Event-ID` is sent explicitly on reconnect, so a resume replays exactly
 *     what was missed — the one recovery mechanism the design has (§8.1);
 *   * the reconnect policy is ours (backoff, jitter, a status the UI can show)
 *     rather than the browser's three-second default;
 *   * a `401` mid-stream refreshes the token and reconnects instead of dying.
 *
 * What is given up is the browser's automatic reconnect, which is reimplemented
 * below and needed to be anyway, because the token has to be refreshed with it.
 */

import { useSession } from "@/auth/session";
import { apiBaseUrl } from "@/config";
import type { Event as StreamEvent } from "./generated/types.gen";

/** One decoded SSE frame, before its `data` is parsed. */
export interface Frame {
  /** The `id:` line — the cursor, and what `Last-Event-ID` replays. */
  id?: string;
  /** The `event:` line. Defaults to `message`, as the spec says. */
  event: string;
  /** The `data:` lines, joined with newlines. */
  data: string;
}

/**
 * An incremental SSE decoder.
 *
 * Stateful because a chunk boundary lands wherever TCP decides: half a frame now
 * and the rest in 40 ms is normal, and a parser that assumed whole frames would
 * drop events under exactly the load where they matter.
 */
export function createFrameParser(): (chunk: string) => Frame[] {
  let buffer = "";

  return (chunk: string): Frame[] => {
    buffer += chunk.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
    const frames: Frame[] = [];

    let boundary = buffer.indexOf("\n\n");
    while (boundary !== -1) {
      const block = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      const frame = decodeBlock(block);
      if (frame) {
        frames.push(frame);
      }
      boundary = buffer.indexOf("\n\n");
    }

    return frames;
  };
}

function decodeBlock(block: string): Frame | null {
  let id: string | undefined;
  let event = "message";
  const data: string[] = [];

  for (const line of block.split("\n")) {
    // A line starting with a colon is a comment — that is what the 25-second
    // keep-alive is (§8.1). It carries no id, so it never moves the cursor.
    if (line === "" || line.startsWith(":")) {
      continue;
    }
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    const value =
      colon === -1
        ? ""
        : line.slice(line[colon + 1] === " " ? colon + 2 : colon + 1);

    if (field === "id") {
      id = value;
    } else if (field === "event") {
      event = value;
    } else if (field === "data") {
      data.push(value);
    }
    // `retry` is ignored on purpose: the backoff below is ours.
  }

  return data.length ? { id, event, data: data.join("\n") } : null;
}

/** What the shell shows next to the channel list. */
export type StreamStatus = "connecting" | "live" | "reconnecting" | "offline";

export interface StreamHandlers {
  /** One event, already parsed. */
  onEvent: (event: StreamEvent) => void;
  /** Connection state, for the indicator in the shell. */
  onStatus?: (status: StreamStatus) => void;
}

export interface StreamOptions extends StreamHandlers {
  /** Narrow to these channels. Empty means everything the caller may read. */
  channels?: string[];
  /** Where to resume from on the very first connection. */
  cursor?: string;
}

/** A running stream. Call `close()` and it stops, for good. */
export interface Stream {
  close: () => void;
  /** The last cursor seen, so a replacement stream can resume where this one was. */
  lastEventId: () => string | undefined;
}

/** Backoff, capped: a server coming back must not be met with a thundering herd. */
const BACKOFF_MS = [1_000, 2_000, 5_000, 10_000, 30_000];

/**
 * Silence after which a connection is presumed dead.
 *
 * The server sends a keep-alive every 25 s (§8.1), so two missed ones is not a
 * quiet channel — it is a socket that died without a FIN (laptop sleep, a NAT
 * that forgot us), on which `read()` would otherwise wait for ever while the
 * indicator says "live".
 */
export const IDLE_MS = 60_000;

/**
 * Open the stream and keep it open.
 *
 * The returned handle is the only way to stop it; everything else — token
 * refresh, resume, backoff — is handled inside.
 */
export function openStream(options: StreamOptions): Stream {
  const session = useSession();
  const controller = new AbortController();
  let closed = false;
  let attempt = 0;
  /** Whether this attempt already spent its one free token refresh. */
  let refreshed = false;
  /** The last cursor seen. This is what makes a reconnect a resume. */
  let lastEventId = options.cursor;

  const status = (value: StreamStatus) => options.onStatus?.(value);

  /** Cuts a backoff short: set while `connect` is waiting to retry. */
  let wake: (() => void) | null = null;
  // Back online, or the tab shown again: whatever the backoff was waiting out has
  // probably passed, and the person looking at the screen should not wait 30 s.
  const nudge = () => {
    if (document.visibilityState === "visible") {
      wake?.();
    }
  };
  window.addEventListener("online", nudge);
  document.addEventListener("visibilitychange", nudge);

  async function connect(): Promise<void> {
    while (!closed) {
      try {
        status(attempt === 0 ? "connecting" : "reconnecting");

        const token = await session.bearer();
        if (!token) {
          // No session left; the router is about to send us to the provider and
          // reconnecting in a loop would only burn a request per second doing it.
          status("offline");
          return;
        }

        const url = new URL(`${apiBaseUrl}/api/v1/stream`);
        if (options.channels?.length) {
          url.searchParams.set("channels", options.channels.join(","));
        }

        const headers: Record<string, string> = {
          Authorization: `Bearer ${token}`,
          Accept: "text/event-stream",
        };
        if (lastEventId) {
          headers["Last-Event-ID"] = lastEventId;
        }

        const response = await fetch(url, {
          headers,
          signal: controller.signal,
          // No credentials: the API is bearer-only and CORS is closed (§5.1).
          credentials: "omit",
        });

        if (response.status === 401) {
          if (!(await session.refresh())) {
            status("offline");
            return;
          }
          // Exactly one immediate retry, and then never again without backoff.
          // A `401` that survives a successful refresh is not a stale token — it
          // is clock skew, a revoked account, an audience mismatch — and retrying
          // it immediately in a loop is a client hammering its own API as fast as
          // the network allows, refreshing a token every round.
          if (refreshed) {
            throw new Error("still refused after a refresh");
          }
          refreshed = true;
          continue;
        }
        refreshed = false;

        if (!response.ok || !response.body) {
          throw new Error(`stream refused with ${response.status}`);
        }

        attempt = 0;
        status("live");
        await pump(response.body);
      } catch (error) {
        if (closed || controller.signal.aborted) {
          return;
        }
        // An ordinary end of stream and a network failure look the same from
        // here, and are handled the same: wait, then resume from the cursor.
        console.debug("stream interrupted, reconnecting", error);
      }

      if (closed) {
        return;
      }

      status("reconnecting");
      const wait = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
      attempt += 1;
      // Jitter, so a replica restart does not bring every tab back in lockstep.
      await new Promise<void>((resolve) => {
        wake = resolve;
        void sleep(wait * (0.5 + Math.random() / 2), controller.signal).then(
          resolve,
        );
      });
      wake = null;
    }
  }

  async function pump(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    const parse = createFrameParser();

    let idle: ReturnType<typeof setTimeout> | undefined;
    const watch = () => {
      clearTimeout(idle);
      // Cancelling makes the pending `read()` resolve `done`, which is the
      // ordinary end-of-stream path: back off, then resume from the cursor.
      idle = setTimeout(() => void reader.cancel().catch(() => {}), IDLE_MS);
    };

    try {
      watch();
      while (!closed) {
        const { done, value } = await reader.read();
        if (done) {
          return;
        }
        watch();
        ingest(decoder.decode(value, { stream: true }));
      }
    } finally {
      clearTimeout(idle);
    }

    function ingest(chunk: string): void {
      for (const frame of parse(chunk)) {
        if (frame.id) {
          lastEventId = frame.id;
        }
        try {
          options.onEvent(JSON.parse(frame.data) as StreamEvent);
        } catch (error) {
          // One unparseable frame must not take the connection down with it.
          console.warn("dropping an unreadable stream frame", error);
        }
      }
    }
  }

  void connect();

  return {
    close() {
      closed = true;
      controller.abort();
      window.removeEventListener("online", nudge);
      document.removeEventListener("visibilitychange", nudge);
      status("offline");
    },
    lastEventId: () => lastEventId,
  };
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal.addEventListener("abort", onAbort, { once: true });
  });
}
