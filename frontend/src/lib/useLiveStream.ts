import * as React from "react";
import { ApiError } from "./api";
import { streamLiveSession } from "./liveStream";
import { evictLiveCache, getLiveCache, maxSeq } from "./liveMessageCache";
import {
  isPureHiddenThinkingMessage,
  isPureToolResultMessage,
} from "./toolResults";
import type { Message, Role } from "./types";

/** Reconnect backoff schedule (ms): 1s → 2s → 5s, then hold at 5s. */
const BACKOFF_MS = [1000, 2000, 5000];

/** Connection status for the live view. */
export type LiveStatus =
  /** Actively reading the stream. */
  | { kind: "streaming" }
  /** Transport dropped; waiting to reconnect (server replays the backlog). */
  | { kind: "reconnecting" }
  /** Terminal `complete` frame — the session graduated. Carries its id. */
  | { kind: "complete"; sessionId: string }
  /** Terminal error (4xx or unrecoverable). */
  | { kind: "error"; message: string };

/** What the transcript would render, known without downloading bodies. */
export interface TranscriptOutline {
  /** Number of messages the transcript renders. */
  count: number;
  /** Role of the last rendered message; `undefined` when there is none. */
  lastRole: Role | undefined;
}

export interface UseLiveStreamResult {
  /** Messages sorted ASCENDING by envelope `seq`; empty without bodies. */
  messages: Message[];
  outline: TranscriptOutline;
  status: LiveStatus;
}

interface OutlineEntry {
  role: Role;
  visible: boolean;
}

/**
 * Subscribe to `GET /api/live/:id/stream` and expose the conversation as it
 * streams. Messages are kept in a `Map<seq, Message>` and returned SORTED by
 * `seq` (a reconnect replays the backlog and arrival order is not guaranteed; a
 * duplicate `seq` just overwrites, never double-renders).
 *
 * The `seq → Message` map is the per-session entry from the SPA-lifetime
 * {@link getLiveCache}, so switching between live sessions (which changes the
 * `:id` param and re-runs this effect) paints the revisited session INSTANTLY
 * from cache instead of waiting for a full re-download. Each connect/reconnect
 * passes `since = maxSeq(cache)` so the server replays only the tail; the empty
 * cache of a first visit / cold reload omits `since` and gets the full backlog.
 * A terminal `complete` frame evicts the session (it graduated).
 *
 * Reconnect is bounded and terminal-aware: it retries ONLY on a transport drop
 * (EOF / non-4xx error) with a capped backoff, and NEVER after a `complete`
 * frame or a 4xx response. Unmount aborts the in-flight fetch.
 */
export function useLiveStream(
  id: string,
  /**
   * Fired when the server links one or more new PR refs for this session (a
   * `prs_linked` frame). Use it to refetch the PR list so the widgets update
   * live. Held in a ref so a changing callback identity never resubscribes.
   */
  onPrsLinked?: () => void,
  /**
   * Fired on a `linear_docs_linked` frame — the Linear analogue of
   * `onPrsLinked`. Use it to refetch the session's Linear list.
   */
  onLinearDocsLinked?: () => void,
  onPrCommentsChanged?: () => void,
  /**
   * `false` streams only the message outline (`?bodies=false`), for a
   * collapsed transcript. Flipping it to `true` reopens the stream with bodies.
   */
  { bodies = true }: { bodies?: boolean } = {},
): UseLiveStreamResult {
  // seq → Message. A ref holds the source of truth; a version counter triggers
  // re-renders so we don't allocate a new Map object on every single frame.
  // Both maps are tagged with their session: after a switch, the render before
  // the new subscription's effect must not read the previous session's maps.
  // The outline is kept after switching to bodies so it stays known until the
  // body replay arrives.
  const bySeq = React.useRef({ id, entries: new Map<number, Message>() });
  const outlineBySeq = React.useRef({
    id,
    entries: new Map<number, OutlineEntry>(),
  });
  const [, bump] = React.useReducer((n: number) => n + 1, 0);
  const [status, setStatus] = React.useState<LiveStatus>({ kind: "streaming" });

  // Keep the latest callback in a ref so the subscribe effect (keyed only on
  // `id`) reads the current one without tearing down the stream on every render.
  const onPrsLinkedRef = React.useRef(onPrsLinked);
  onPrsLinkedRef.current = onPrsLinked;
  const onLinearDocsLinkedRef = React.useRef(onLinearDocsLinked);
  onLinearDocsLinkedRef.current = onLinearDocsLinked;
  const onPrCommentsChangedRef = React.useRef(onPrCommentsChanged);
  onPrCommentsChangedRef.current = onPrCommentsChanged;

  React.useEffect(() => {
    if (!id) return;
    const controller = new AbortController();
    let cancelled = false;
    let attempt = 0;

    // Point at this session's cached map so a re-visit seeds instantly. Writes
    // below mutate the cache entry itself (it's a live reference, not a copy).
    // Only this subscription writes to these maps while it is current; a frame
    // that a cancelled subscription still delivers is dropped. Collapsing the
    // same session again keeps its outline and resumes it from the tail.
    const bodyEntries = bodies ? getLiveCache(id) : null;
    const outlineEntries =
      outlineBySeq.current.id === id
        ? outlineBySeq.current.entries
        : new Map<number, OutlineEntry>();
    // Bodies loaded while expanded may be newer than the outline; record them
    // so the outline resumes after them instead of losing them.
    if (!bodies && bySeq.current.id === id) {
      for (const [seq, message] of bySeq.current.entries) {
        outlineEntries.set(seq, outlineEntry(message));
      }
    }
    if (bodyEntries) {
      bySeq.current = { id, entries: bodyEntries };
    } else {
      outlineBySeq.current = { id, entries: outlineEntries };
    }
    bump();
    setStatus({ kind: "streaming" });

    const run = async () => {
      while (!cancelled) {
        try {
          // Resume from the highest seq we already hold (cache + this session's
          // prior frames) so the server replays only the tail. `undefined` on a
          // cold cache ⇒ full replay. Recomputed each attempt so a reconnect
          // after new frames arrived doesn't re-replay them.
          const since = maxSeq(bodyEntries ?? outlineEntries);
          const end = await streamLiveSession(
            id,
            {
              onMessage: (seq, frame) => {
                if (cancelled || !bodyEntries) return;
                bodyEntries.set(seq, frame.message);
                bump();
              },
              onOutline: ({ seq, role, visible }) => {
                if (cancelled || bodyEntries) return;
                outlineEntries.set(seq, { role, visible });
                bump();
              },
              onPrsLinked: () => {
                if (!cancelled) onPrsLinkedRef.current?.();
              },
              onPrCommentsChanged: () => {
                if (!cancelled) onPrCommentsChangedRef.current?.();
              },
              onLinearDocsLinked: () => {
                if (!cancelled) onLinearDocsLinkedRef.current?.();
              },
            },
            controller.signal,
            since,
            { bodies },
          );
          if (cancelled) return;
          if (end.reason === "complete") {
            // Graduated: the live id is done — drop its cache entry.
            evictLiveCache(id);
            setStatus({ kind: "complete", sessionId: end.sessionId });
            return;
          }
          if (end.reason === "aborted") return;
          // reason === "eof": transport drop — fall through to reconnect.
        } catch (err) {
          if (cancelled) return;
          // 4xx is terminal: the stream is gone / forbidden, no point retrying.
          if (
            err instanceof ApiError &&
            err.status >= 400 &&
            err.status < 500
          ) {
            setStatus({ kind: "error", message: err.message });
            return;
          }
          // Otherwise a transport/server blip — reconnect with backoff.
        }

        if (cancelled) return;
        setStatus({ kind: "reconnecting" });
        const delay = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
        attempt += 1;
        await new Promise((r) => setTimeout(r, delay));
        if (cancelled) return;
        setStatus({ kind: "streaming" });
      }
    };

    void run();

    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [id, bodies]);

  // `bump()` forces a render on every frame, so deriving here each render
  // (a sort over the small live message set) is fine and always current.
  const loadedBodies: Map<number, Message> =
    bySeq.current.id === id ? bySeq.current.entries : new Map();
  const messages = bodies
    ? [...loadedBodies.entries()].sort((a, b) => a[0] - b[0]).map(([, m]) => m)
    : [];
  // The outline is everything known about this session: the outline frames
  // plus any loaded bodies. Neither a partial body replay nor a stale outline
  // after collapsing can then lower the count or roll back the latest role.
  const known = new Map<number, OutlineEntry>(
    outlineBySeq.current.id === id ? outlineBySeq.current.entries : [],
  );
  for (const [seq, message] of loadedBodies) {
    known.set(seq, outlineEntry(message));
  }
  return { messages, outline: outlineOf(known), status };
}

/** The outline of a loaded message, matching `filterRenderedMessages`. */
function outlineEntry(message: Message): OutlineEntry {
  return {
    role: message.role,
    visible:
      !isPureToolResultMessage(message) &&
      !isPureHiddenThinkingMessage(message),
  };
}

function outlineOf(entries: Map<number, OutlineEntry>): TranscriptOutline {
  const visible = [...entries.entries()]
    .filter(([, entry]) => entry.visible)
    .sort((a, b) => a[0] - b[0]);
  return { count: visible.length, lastRole: visible.at(-1)?.[1].role };
}
