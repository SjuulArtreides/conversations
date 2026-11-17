import { afterEach, expect, it, vi } from "vitest";
import { streamLiveSession } from "./liveStream";

afterEach(() => vi.unstubAllGlobals());

it("dispatches a PR comment change without treating it as a conversation message", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(
      new Response('data: {"type":"pr_comments_changed"}\n\n', {
        headers: { "content-type": "text/event-stream" },
      }),
    ),
  );
  const onMessage = vi.fn();
  const onPrCommentsChanged = vi.fn();
  const end = await streamLiveSession(
    "session-1",
    { onMessage, onPrCommentsChanged },
    undefined,
    12,
  );

  expect(end).toEqual({ reason: "eof" });
  expect(fetch).toHaveBeenCalledWith(
    "/api/live/session-1/stream?since=12",
    expect.anything(),
  );
  expect(onPrCommentsChanged).toHaveBeenCalledTimes(1);
  expect(onMessage).not.toHaveBeenCalled();
});
