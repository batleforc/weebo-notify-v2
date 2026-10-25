import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const session = vi.hoisted(() => ({
  expired: false,
  bearer: vi.fn<() => Promise<string | null>>(),
  refresh: vi.fn<() => Promise<string | null>>(),
  canResume: vi.fn<() => boolean>(),
}));

vi.mock("@/auth/session", () => ({ useSession: () => session }));

import { createFrameParser, openStream, type StreamStatus } from "./stream";

/** The SSE decoder against real server output (§8.1): keep-alives, cursors, split frames. */
describe("createFrameParser", () => {
  it("decodes one frame", () => {
    const parse = createFrameParser();
    const frames = parse('id: evt_42\nevent: revision\ndata: {"seq":1}\n\n');

    expect(frames).toEqual([
      { id: "evt_42", event: "revision", data: '{"seq":1}' },
    ]);
  });

  it("keeps half a frame until the rest arrives", () => {
    const parse = createFrameParser();

    expect(parse("id: evt_1\nevent: rev")).toEqual([]);
    expect(parse('ision\ndata: {"a":')).toEqual([]);
    expect(parse("1}\n\n")).toEqual([
      { id: "evt_1", event: "revision", data: '{"a":1}' },
    ]);
  });

  it("decodes several frames out of one chunk", () => {
    const parse = createFrameParser();
    const frames = parse(
      "id: 1\nevent: revision\ndata: a\n\nid: 2\nevent: tombstone\ndata: b\n\n",
    );

    expect(frames.map((frame) => frame.id)).toEqual(["1", "2"]);
    expect(frames.map((frame) => frame.event)).toEqual([
      "revision",
      "tombstone",
    ]);
  });

  it("ignores the keep-alive, which carries no id and must not move the cursor", () => {
    const parse = createFrameParser();

    expect(parse(": ping\n\n")).toEqual([]);
    expect(parse("id: 7\nevent: revision\ndata: x\n\n")).toEqual([
      { id: "7", event: "revision", data: "x" },
    ]);
  });

  it("joins multi-line data the way the spec says", () => {
    const parse = createFrameParser();

    expect(
      parse("event: revision\ndata: line one\ndata: line two\n\n"),
    ).toEqual([
      { id: undefined, event: "revision", data: "line one\nline two" },
    ]);
  });

  it("defaults the event name and tolerates CRLF", () => {
    const parse = createFrameParser();

    expect(parse("data: bare\r\n\r\n")).toEqual([
      { id: undefined, event: "message", data: "bare" },
    ]);
  });

  it("keeps a value that contains a colon intact", () => {
    const parse = createFrameParser();
    const [frame] = parse('event: revision\ndata: {"url":"https://x/y"}\n\n');

    expect(frame?.data).toBe('{"url":"https://x/y"}');
  });
});

/**
 * The reconnect loop: what it does when the token, the server or the network
 * fails. Fake timers and `Math.random` pinned to 1, so every backoff waits its
 * full step (1 s, 2 s, 5 s…) and the arithmetic below is exact.
 */
describe("openStream", () => {
  let fetchMock: ReturnType<typeof vi.fn>;
  let statuses: StreamStatus[];

  /** A `200` whose body sends `chunks` and then ends. */
  function streamed(...chunks: string[]): Response {
    const encoder = new TextEncoder();
    return new Response(
      new ReadableStream({
        start(controller) {
          for (const chunk of chunks) {
            controller.enqueue(encoder.encode(chunk));
          }
          controller.close();
        },
      }),
      { status: 200 },
    );
  }

  function open() {
    return openStream({
      onEvent: () => undefined,
      onStatus: (status) => statuses.push(status),
    });
  }

  beforeEach(() => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(1);
    vi.spyOn(console, "debug").mockImplementation(() => undefined);
    statuses = [];
    session.expired = false;
    session.bearer.mockReset().mockResolvedValue("token-1");
    session.refresh.mockReset().mockResolvedValue("token-2");
    session.canResume.mockReset().mockReturnValue(true);
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("keeps retrying when the token is missing only for now", async () => {
    // Offline refresh: no token, but the refresh token is still held.
    session.bearer.mockResolvedValueOnce(null);
    fetchMock.mockImplementation(() => new Promise(() => undefined));

    const stream = open();
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(statuses).not.toContain("offline");

    await vi.advanceTimersByTimeAsync(1_000);
    expect(fetchMock).toHaveBeenCalledOnce();
    stream.close();
  });

  it("stops for good once the session is over", async () => {
    session.bearer.mockResolvedValue(null);
    session.canResume.mockReturnValue(false);

    open();
    await vi.advanceTimersByTimeAsync(60_000);

    expect(fetchMock).not.toHaveBeenCalled();
    expect(statuses.at(-1)).toBe("offline");
  });

  it("refreshes once on a 401 and reconnects with the new token", async () => {
    fetchMock
      .mockResolvedValueOnce(new Response(null, { status: 401 }))
      .mockImplementation(() => new Promise(() => undefined));

    const stream = open();
    await vi.advanceTimersByTimeAsync(0);

    expect(session.refresh).toHaveBeenCalledOnce();
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(session.bearer).toHaveBeenCalledTimes(2);
    stream.close();
  });

  it("backs off instead of looping when a 401 survives the refresh", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 401 }));

    const stream = open();
    await vi.advanceTimersByTimeAsync(0);
    // The free retry, then a refusal that has to wait.
    expect(fetchMock).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(999);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    stream.close();
  });

  it("keeps backing off on a server that accepts and hangs up", async () => {
    // Headers OK and nothing else: not proof of health, so no reset.
    fetchMock.mockImplementation(async () => streamed());

    const stream = open();
    // Connects at 0, then waits 1 s, 2 s, 5 s: attempts at 0, 1000, 3000, 8000.
    await vi.advanceTimersByTimeAsync(3_500);
    expect(fetchMock).toHaveBeenCalledTimes(3);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(fetchMock).toHaveBeenCalledTimes(4);
    stream.close();
  });

  it("starts the backoff over after a stream that delivered an event", async () => {
    fetchMock.mockImplementation(async () =>
      streamed('id: 1\nevent: revision\ndata: {"seq":1}\n\n'),
    );

    const stream = open();
    // Every attempt delivered, so every wait is the first step: 0, 1000, 2000…
    await vi.advanceTimersByTimeAsync(3_500);
    expect(fetchMock).toHaveBeenCalledTimes(4);
    stream.close();
  });

  it("resumes from the last cursor it saw", async () => {
    fetchMock
      .mockImplementationOnce(async () =>
        streamed('id: evt_9\nevent: revision\ndata: {"seq":1}\n\n'),
      )
      .mockImplementation(() => new Promise(() => undefined));

    const stream = open();
    await vi.advanceTimersByTimeAsync(1_000);

    const [, init] = fetchMock.mock.calls[1] as [URL, RequestInit];
    expect((init.headers as Record<string, string>)["Last-Event-ID"]).toBe(
      "evt_9",
    );
    stream.close();
  });
});
