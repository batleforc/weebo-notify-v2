import { describe, expect, it } from "vitest";

import { createFrameParser } from "./stream";

/**
 * The SSE decoder, which is where a real-time client quietly loses events.
 *
 * Every case below is one the server actually produces (design §8.1): the
 * 25-second keep-alive comment, an `id:` that is the cursor, and — the one that
 * only shows up under load — a frame split across two network chunks.
 */
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

    expect(frame.data).toBe('{"url":"https://x/y"}');
  });
});
