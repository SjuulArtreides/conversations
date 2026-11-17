import { describe, expect, it } from "vitest";
import { consumeSseFrames, parseSseDataFrame, SseFrameSplitter } from "./sse";

interface Frame {
  type: string;
  n?: number;
}

const parse = (f: string) => parseSseDataFrame<Frame>(f);

describe("parseSseDataFrame", () => {
  it("parses a single data line", () => {
    expect(parse('data: {"type":"a","n":1}')).toEqual({ type: "a", n: 1 });
  });

  it("joins multiple data lines with a newline before JSON.parse", () => {
    expect(parse('data: {"type":"a",\ndata: "n":2}')).toEqual({
      type: "a",
      n: 2,
    });
  });

  it("tolerates CRLF and a missing space after the colon", () => {
    expect(parse('data:{"type":"b"}\r')).toEqual({ type: "b" });
  });

  it("returns null for a comment/keep-alive or empty frame", () => {
    expect(parse(": keep-alive")).toBeNull();
    expect(parse("")).toBeNull();
  });
});

describe("SseFrameSplitter", () => {
  it("emits each complete frame in a multi-frame chunk", () => {
    const s = new SseFrameSplitter<Frame>(parse);
    expect(s.push('data: {"type":"a"}\n\ndata: {"type":"b"}\n\n')).toEqual<
      Frame[]
    >([{ type: "a" }, { type: "b" }]);
  });

  it("buffers a frame split across two pushes", () => {
    const s = new SseFrameSplitter<Frame>(parse);
    expect(s.push('data: {"type":"sp')).toEqual([]);
    expect(s.push('lit"}\n\n')).toEqual<Frame[]>([{ type: "split" }]);
  });

  it("flush yields a final frame with no trailing blank line", () => {
    const s = new SseFrameSplitter<Frame>(parse);
    expect(s.push('data: {"type":"last"}')).toEqual([]);
    expect(s.flush()).toEqual<Frame[]>([{ type: "last" }]);
  });

  it("skips frames the parser maps to null", () => {
    const s = new SseFrameSplitter<Frame>(parse);
    expect(s.push(': ping\n\ndata: {"type":"x"}\n\n')).toEqual<Frame[]>([
      { type: "x" },
    ]);
  });

  it("normalizes CRLF frame separators", () => {
    const s = new SseFrameSplitter<Frame>(parse);
    expect(s.push('data: {"type":"a"}\r\n\r\n')).toEqual<Frame[]>([
      { type: "a" },
    ]);
  });
});

describe("consumeSseFrames", () => {
  function body(chunks: Uint8Array[]) {
    return new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    });
  }

  it("decodes split UTF-8 and flushes a trailing frame, releasing the reader", async () => {
    const bytes = new TextEncoder().encode('data: {"type":"é"}');
    const split = bytes.indexOf(0xc3) + 1;
    const stream = body([bytes.slice(0, split), bytes.slice(split)]);
    const frames: Frame[] = [];
    expect(
      await consumeSseFrames(stream, new SseFrameSplitter(parse), (frame) => {
        frames.push(frame);
        return null;
      }),
    ).toBeNull();
    expect(frames).toEqual([{ type: "é" }]);
    expect(stream.locked).toBe(false);
  });

  it("stops at a terminal frame without dispatching later frames", async () => {
    const stream = body([
      new TextEncoder().encode(
        'data: {"type":"done"}\n\ndata: {"type":"late"}\n\n',
      ),
    ]);
    const frames: Frame[] = [];
    expect(
      await consumeSseFrames(stream, new SseFrameSplitter(parse), (frame) => {
        frames.push(frame);
        return "finished";
      }),
    ).toBe("finished");
    expect(frames).toEqual([{ type: "done" }]);
    expect(stream.locked).toBe(false);
  });

  it("propagates transport failures and releases the reader", async () => {
    const failure = new Error("connection lost");
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.error(failure);
      },
    });
    await expect(
      consumeSseFrames(stream, new SseFrameSplitter(parse), () => null),
    ).rejects.toBe(failure);
    expect(stream.locked).toBe(false);
  });
});
