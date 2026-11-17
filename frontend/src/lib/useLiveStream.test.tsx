import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { render, waitFor } from "@testing-library/react";
import { __resetLiveCacheForTest } from "./liveMessageCache";
import { useLiveStream, type UseLiveStreamResult } from "./useLiveStream";
import type { Message } from "./types";

function body(uuid: string, role: "user" | "assistant", text: string): Message {
  return {
    uuid,
    parent_uuid: null,
    role,
    kind: "text",
    model: null,
    is_sidechain: false,
    timestamp: null,
    blocks: [{ type: "text", text }],
    text,
    usage: null,
  };
}

function sse(frames: object[]): Response {
  const encoder = new TextEncoder();
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        for (const frame of frames) {
          controller.enqueue(
            encoder.encode(`data: ${JSON.stringify(frame)}\n\n`),
          );
        }
      },
    }),
    { headers: { "content-type": "text/event-stream" } },
  );
}

const A_MESSAGES = [
  body("a0", "user", "A question"),
  body("a1", "user", "A follow-up"),
  body("a2", "assistant", "A answer"),
];

// Session B's body request never answers, so any body on screen for B must
// have come from somewhere else.
const fetchMock = vi.fn((input: RequestInfo | URL) => {
  const url = String(input);
  if (url.startsWith("/api/live/A/stream")) {
    return Promise.resolve(
      sse(
        A_MESSAGES.map((message, seq) => ({ type: "message", seq, message })),
      ),
    );
  }
  if (url === "/api/live/B/stream?bodies=false") {
    return Promise.resolve(
      sse([{ type: "message_outline", seq: 0, role: "user", visible: true }]),
    );
  }
  if (url === "/api/live/C/stream?bodies=false") {
    return Promise.resolve(
      sse(
        A_MESSAGES.map((message, seq) => ({
          type: "message_outline",
          seq,
          role: message.role,
          visible: true,
        })),
      ),
    );
  }
  // C's body replay delivers only its first message, then stalls.
  if (url === "/api/live/C/stream") {
    return Promise.resolve(
      sse([{ type: "message", seq: 0, message: A_MESSAGES[0] }]),
    );
  }
  // D: the outline knows two messages; the body stream then also delivers a
  // newer user message that the outline has not seen.
  if (url === "/api/live/D/stream?bodies=false") {
    return Promise.resolve(
      sse(
        A_MESSAGES.slice(1).map((message, seq) => ({
          type: "message_outline",
          seq,
          role: message.role,
          visible: true,
        })),
      ),
    );
  }
  if (url === "/api/live/D/stream") {
    return Promise.resolve(
      sse(
        [
          A_MESSAGES[1],
          A_MESSAGES[2],
          body("d2", "user", "D new question"),
        ].map((message, seq) => ({ type: "message", seq, message })),
      ),
    );
  }
  return new Promise<Response>(() => {});
});

const renders: { id: string; result: UseLiveStreamResult }[] = [];

function Probe({ id, bodies }: { id: string; bodies: boolean }) {
  const result = useLiveStream(id, undefined, undefined, undefined, {
    bodies,
  });
  renders.push({ id, result });
  return null;
}

beforeEach(() => {
  __resetLiveCacheForTest();
  renders.length = 0;
  fetchMock.mockClear();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

it("never shows one session's bodies or outline on another session", async () => {
  const { rerender } = render(<Probe id="A" bodies />);
  await waitFor(() => expect(renders.at(-1)?.result.messages).toHaveLength(3));

  rerender(<Probe id="B" bodies={false} />);
  await waitFor(() => expect(renders.at(-1)?.result.outline.count).toBe(1));
  rerender(<Probe id="B" bodies />);
  await waitFor(() =>
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/live/B/stream",
      expect.anything(),
    ),
  );

  for (const { result } of renders.filter((r) => r.id === "B")) {
    expect(result.messages).toEqual([]);
    expect(result.outline.count).toBeLessThanOrEqual(1);
    expect(result.outline.lastRole).not.toBe("assistant");
  }
});

it("keeps the complete outline while a cold body replay catches up", async () => {
  const { rerender } = render(<Probe id="C" bodies={false} />);
  await waitFor(() => expect(renders.at(-1)?.result.outline.count).toBe(3));

  rerender(<Probe id="C" bodies />);
  await waitFor(() => expect(renders.at(-1)?.result.messages).toHaveLength(1));

  for (const { result } of renders.slice(renders.length - 3)) {
    expect(result.outline).toEqual({ count: 3, lastRole: "assistant" });
  }
});

it("keeps the outline when a session is collapsed again", async () => {
  const { rerender } = render(<Probe id="C" bodies={false} />);
  await waitFor(() => expect(renders.at(-1)?.result.outline.count).toBe(3));
  rerender(<Probe id="C" bodies />);
  await waitFor(() => expect(renders.at(-1)?.result.messages).toHaveLength(1));
  const collapsedAt = renders.length;

  rerender(<Probe id="C" bodies={false} />);
  await waitFor(() =>
    expect(fetchMock).toHaveBeenLastCalledWith(
      "/api/live/C/stream?since=2&bodies=false",
      expect.anything(),
    ),
  );

  for (const { result } of renders.slice(collapsedAt)) {
    expect(result.outline).toEqual({ count: 3, lastRole: "assistant" });
  }
});

it("keeps what loaded bodies revealed when collapsing again", async () => {
  const { rerender } = render(<Probe id="D" bodies={false} />);
  await waitFor(() =>
    expect(renders.at(-1)?.result.outline).toEqual({
      count: 2,
      lastRole: "assistant",
    }),
  );
  rerender(<Probe id="D" bodies />);
  await waitFor(() => expect(renders.at(-1)?.result.messages).toHaveLength(3));
  const collapsedAt = renders.length;

  rerender(<Probe id="D" bodies={false} />);
  await waitFor(() =>
    expect(fetchMock).toHaveBeenLastCalledWith(
      "/api/live/D/stream?since=2&bodies=false",
      expect.anything(),
    ),
  );

  for (const { result } of renders.slice(collapsedAt)) {
    expect(result.outline).toEqual({ count: 3, lastRole: "user" });
  }
  expect(
    fetchMock.mock.calls.filter(([url]) => url === "/api/live/D/stream"),
  ).toHaveLength(1);
});
