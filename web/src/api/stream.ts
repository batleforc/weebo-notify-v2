/**
 * The live stream (§8.1), read with `fetch` because `EventSource` can't send an
 * `Authorization` header. Reconnect, `Last-Event-ID` resume and backoff are ours.
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

/** Incremental SSE decoder: chunks split frames anywhere. */
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
    // Comment lines are the 15 s keep-alive (§8.1); they never move the cursor.
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
export type StreamStatus =
  | "connecting"
  | "live"
  | "reconnecting"
  | "offline"
  | "disconnected";

export interface StreamOptions {
  onEvent: (event: StreamEvent) => void;
  onStatus?: (status: StreamStatus) => void;
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

/** Four missed 15 s keep-alives (§8.1): the socket died without a FIN. */
export const IDLE_MS = 60_000;

/** Uptime after which the backoff starts over: a 200 that drops at once isn't healthy. */
export const HEALTHY_MS = 20_000;

/** Thrown inside `connect` when there is no session left to reconnect with. */
const SESSION_OVER = new Error("session over");

/** Open the stream and keep it open; refresh, resume and backoff are internal. */
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
  // Online again or tab visible: skip the rest of the backoff.
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
          ensureResumable();
          throw new Error("no token right now");
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
            ensureResumable();
            throw new Error("refused, and no token right now");
          }
          // One immediate retry only: a 401 after a fresh token is not staleness.
          if (refreshed) {
            throw new Error("still refused after a refresh");
          }
          refreshed = true;
          continue;
        }
        refreshed = false;

        // Its owner cut this device off from another one: retrying would only be refused.
        if (response.status === 403) {
          const problem = await response.json().catch(() => null);
          if (problem?.code === "stream.disconnected") {
            status("disconnected");
            return;
          }
        }

        if (!response.ok || !response.body) {
          throw new Error(`stream refused with ${response.status}`);
        }

        status("live");
        await pump(response.body);
      } catch (error) {
        if (error === SESSION_OVER) {
          // No session: the router is about to redirect to the provider.
          status("offline");
          return;
        }
        if (closed || controller.signal.aborted) {
          return;
        }
        // End of stream or network failure: both back off and resume.
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

  /** No token is only the end without a refresh token; otherwise network/IdP failed: back off. */
  function ensureResumable(): void {
    if (session.expired || !session.canResume()) {
      throw SESSION_OVER;
    }
  }

  async function pump(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const opened = Date.now();
    const decoder = new TextDecoder();
    const parse = createFrameParser();

    let idle: ReturnType<typeof setTimeout> | undefined;
    const watch = () => {
      clearTimeout(idle);
      // Cancelling resolves `read()` as done: the ordinary reconnect path.
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
        // Reset the backoff on a proven stream (an event, or HEALTHY_MS up), not on headers.
        if (
          ingest(decoder.decode(value, { stream: true })) ||
          Date.now() - opened >= HEALTHY_MS
        ) {
          attempt = 0;
        }
      }
    } finally {
      clearTimeout(idle);
    }

    /** Whether at least one event was delivered. */
    function ingest(chunk: string): boolean {
      let delivered = false;
      for (const frame of parse(chunk)) {
        if (frame.id) {
          lastEventId = frame.id;
        }
        try {
          options.onEvent(JSON.parse(frame.data) as StreamEvent);
          delivered = true;
        } catch (error) {
          // One unparseable frame must not take the connection down with it.
          console.warn("dropping an unreadable stream frame", error);
        }
      }
      return delivered;
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
