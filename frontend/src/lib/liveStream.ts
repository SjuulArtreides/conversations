import { errorFromResponse, enc } from "./api";
import { consumeSseFrames, parseSseDataFrame, SseFrameSplitter } from "./sse";
import type { LiveFrame } from "./types";

/** Parse one live-stream SSE frame into a {@link LiveFrame} (or null to skip). */
export function parseLiveFrame(frame: string): LiveFrame | null {
  return parseSseDataFrame<LiveFrame>(frame);
}

/** Why a single {@link streamLiveSession} attempt ended. */
export type LiveStreamEnd =
  /** Server sent the terminal `complete` frame — do NOT reconnect. */
  | { reason: "complete"; sessionId: string }
  /** Stream EOF without a `complete` — a transport drop; safe to reconnect. */
  | { reason: "eof" }
  /** Aborted by the caller (component unmount) — do NOT reconnect. */
  | { reason: "aborted" };

/** Callbacks for one live-stream attempt. */
export interface LiveStreamHandlers {
  /** A `message` frame: upsert `message` keyed by the envelope `seq`. */
  onMessage: (
    seq: number,
    frame: Extract<LiveFrame, { type: "message" }>,
  ) => void;
  /** A `message_outline` frame from a stream opened without bodies. */
  onOutline?: (frame: Extract<LiveFrame, { type: "message_outline" }>) => void;
  /**
   * A `prs_linked` frame: the server linked one or more new PR refs for this
   * session. A refetch nudge — reload the PR list (no payload to apply).
   * Optional so existing callers that don't care about PRs need not handle it.
   */
  onPrsLinked?: () => void;
  onPrCommentsChanged?: () => void;
  /**
   * A `linear_docs_linked` frame: the server linked one or more new Linear refs
   * for this session. A refetch nudge — reload the Linear list. Optional.
   */
  onLinearDocsLinked?: () => void;
}

/**
 * Open `GET /api/live/:id/stream` and consume the `text/event-stream` body via
 * `fetch` + `ReadableStream` (NOT `EventSource`). Resolves with how the attempt
 * ended so the caller can decide whether to reconnect.
 *
 * Terminal cases the caller must NOT reconnect on:
 *  - a `complete` frame (resolves `{reason:"complete"}`),
 *  - the caller's `AbortSignal` firing (resolves `{reason:"aborted"}`),
 *  - a 4xx response (throws {@link ApiError} — caller treats as terminal).
 *
 * A 5xx / network failure / EOF without `complete` is a transport drop:
 * resolves `{reason:"eof"}` (or throws a non-4xx error) so the caller can
 * reconnect; the server replays the backlog and the seq-keyed map dedups it.
 *
 * `since` is an optional resume cursor: when set, the request adds `?since=<seq>`
 * so the server's initial replay carries only messages with `seq > since`
 * (incremental resume from a cached backlog). Omit it for a full replay. The
 * live tail is unaffected — dedup on `seq` still covers any replay/live overlap.
 *
 * `bodies: false` asks for `message_outline` frames instead of `message`
 * frames, so a collapsed transcript downloads no message bodies.
 */
export async function streamLiveSession(
  id: string,
  handlers: LiveStreamHandlers,
  signal?: AbortSignal,
  since?: number,
  { bodies = true }: { bodies?: boolean } = {},
): Promise<LiveStreamEnd> {
  if (signal?.aborted) return { reason: "aborted" };

  const params = new URLSearchParams();
  if (since !== undefined) params.set("since", String(since));
  if (!bodies) params.set("bodies", "false");
  const query = params.toString() ? `?${params}` : "";
  const res = await fetch(`/api/live/${enc(id)}/stream${query}`, {
    headers: { Accept: "text/event-stream" },
    signal,
  });

  if (!res.ok || !res.body) {
    throw await errorFromResponse(res);
  }

  const splitter = new SseFrameSplitter<LiveFrame>(parseLiveFrame);

  // Returns a terminal end-reason when a `complete` frame is seen, else null.
  const dispatch = (frame: LiveFrame): LiveStreamEnd | null => {
    if (frame.type === "complete") {
      return { reason: "complete", sessionId: frame.session_id };
    }
    // `pty` frames are consumed by the separate livePtyStream helper, not here.
    if (frame.type === "pty") {
      return null;
    }
    // `prs_linked` is a refetch nudge — reload the PR list, keep streaming.
    if (frame.type === "prs_linked") {
      handlers.onPrsLinked?.();
      return null;
    }
    if (frame.type === "pr_comments_changed") {
      handlers.onPrCommentsChanged?.();
      return null;
    }
    // `linear_docs_linked` is the same nudge for Linear refs.
    if (frame.type === "linear_docs_linked") {
      handlers.onLinearDocsLinked?.();
      return null;
    }
    if (frame.type === "message_outline") {
      handlers.onOutline?.(frame);
      return null;
    }
    handlers.onMessage(frame.seq, frame);
    return null;
  };

  try {
    const end = await consumeSseFrames(res.body, splitter, dispatch);
    if (end) return end;
  } catch (err) {
    // An abort surfaces as a DOMException/AbortError — treat as a clean stop.
    if (signal?.aborted) return { reason: "aborted" };
    throw err;
  }

  return signal?.aborted ? { reason: "aborted" } : { reason: "eof" };
}
