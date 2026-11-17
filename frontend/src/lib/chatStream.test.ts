import { describe, expect, it, vi } from "vitest";
import {
  parseSseFrame,
  SseParser,
  streamChatMessage,
  type StreamEvent,
} from "./chatStream";
import { ApiError } from "./api";
import type { ChatMessage } from "./types";

const PERSISTED: ChatMessage = {
  id: "m2",
  chat_id: "c1",
  role: "assistant",
  content: "Hello there!",
  created_at: "2026-06-06T00:00:00Z",
};

describe("parseSseFrame", () => {
  it("parses a single data line into an event", () => {
    expect(parseSseFrame('data: {"type":"token","text":"Hi"}')).toEqual({
      type: "token",
      text: "Hi",
    });
  });

  it("joins multiple data lines with a newline before JSON.parse", () => {
    const frame = 'data: {"type":"token",\ndata: "text":"x"}';
    expect(parseSseFrame(frame)).toEqual({ type: "token", text: "x" });
  });

  it("tolerates CRLF and a missing space after the colon", () => {
    const frame = 'data:{"type":"token","text":"y"}\r';
    expect(parseSseFrame(frame)).toEqual({ type: "token", text: "y" });
  });

  it("returns null for a comment/keep-alive frame with no data lines", () => {
    expect(parseSseFrame(": keep-alive")).toBeNull();
    expect(parseSseFrame("")).toBeNull();
  });
});

describe("SseParser", () => {
  it("emits each complete frame in a multi-frame chunk", () => {
    const p = new SseParser();
    const events = p.push(
      'data: {"type":"token","text":"a"}\n\n' +
        'data: {"type":"token","text":"b"}\n\n',
    );
    expect(events).toEqual<StreamEvent[]>([
      { type: "token", text: "a" },
      { type: "token", text: "b" },
    ]);
  });

  it("buffers a frame split across two reads", () => {
    const p = new SseParser();
    // First read carries only part of the frame — no complete event yet.
    expect(p.push('data: {"type":"to')).toEqual([]);
    // Second read completes the frame.
    expect(p.push('ken","text":"split"}\n\n')).toEqual<StreamEvent[]>([
      { type: "token", text: "split" },
    ]);
  });

  it("handles the terminal done frame, including a final frame with no trailing blank line", () => {
    const p = new SseParser();
    const mid = p.push('data: {"type":"token","text":"Hello"}\n\n');
    expect(mid).toEqual<StreamEvent[]>([{ type: "token", text: "Hello" }]);

    // Final `done` frame arrives without a trailing "\n\n" — flush() yields it.
    const doneFrame = `data: ${JSON.stringify({ type: "done", message: PERSISTED })}`;
    expect(p.push(doneFrame)).toEqual([]);
    expect(p.flush()).toEqual<StreamEvent[]>([
      { type: "done", message: PERSISTED },
    ]);
  });

  it("parses a stream where token frames and the done frame share one chunk", () => {
    const p = new SseParser();
    const chunk =
      'data: {"type":"token","text":"Hello "}\n\n' +
      'data: {"type":"token","text":"there!"}\n\n' +
      `data: ${JSON.stringify({ type: "done", message: PERSISTED })}\n\n`;
    expect(p.push(chunk)).toEqual<StreamEvent[]>([
      { type: "token", text: "Hello " },
      { type: "token", text: "there!" },
      { type: "done", message: PERSISTED },
    ]);
  });
});

/** Build a ReadableStream-backed Response body from raw SSE text chunks. */
function streamResponse(chunks: string[]): Response {
  const enc = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks) controller.enqueue(enc.encode(c));
      controller.close();
    },
  });
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

describe("streamChatMessage", () => {
  it("drives onToken for deltas (incl. a split frame) then onDone with the persisted message", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      streamResponse([
        'data: {"type":"token","text":"Hello "}\n\n' + 'data: {"type":"to',
        'ken","text":"there!"}\n\n' +
          `data: ${JSON.stringify({ type: "done", message: PERSISTED })}\n\n`,
      ]),
    );
    vi.stubGlobal("fetch", fetchMock);

    const tokens: string[] = [];
    let done: ChatMessage | null = null;
    const onError = vi.fn();
    await streamChatMessage("c1", "hi", {
      onToken: (t) => tokens.push(t),
      onDone: (m) => (done = m),
      onError,
    });

    expect(tokens).toEqual(["Hello ", "there!"]);
    expect(done).toEqual(PERSISTED);
    expect(onError).not.toHaveBeenCalled();

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/api/chats/c1/messages");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual({ content: "hi" });
    expect(init.headers.Accept).toBe("text/event-stream");

    vi.unstubAllGlobals();
  });

  it("calls onError on an error frame and stops", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        streamResponse([
          'data: {"type":"error","message":"rate limited"}\n\n',
        ]),
      );
    vi.stubGlobal("fetch", fetchMock);

    const onDone = vi.fn();
    const onError = vi.fn();
    await streamChatMessage("c1", "hi", {
      onToken: vi.fn(),
      onDone,
      onError,
    });

    expect(onError).toHaveBeenCalledWith("rate limited");
    expect(onDone).not.toHaveBeenCalled();
    vi.unstubAllGlobals();
  });

  it("throws ApiError on a non-OK response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ error: "not visible" }), {
          status: 404,
          headers: { "content-type": "application/json" },
        }),
      ),
    );
    await expect(
      streamChatMessage("c1", "hi", {
        onToken: vi.fn(),
        onDone: vi.fn(),
        onError: vi.fn(),
      }),
    ).rejects.toBeInstanceOf(ApiError);
    vi.unstubAllGlobals();
  });
});
