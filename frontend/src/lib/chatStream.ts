import { errorFromResponse, enc } from "./api";
import { consumeSseFrames, parseSseDataFrame, SseFrameSplitter } from "./sse";
import type { ChatMessage } from "./types";

/**
 * One decoded SSE event from the streaming-message endpoint.
 *
 * The server emits incremental `token` frames, a terminal `done` frame whose
 * `message` is the persisted assistant `ChatMessage`, or an `error` frame.
 */
export type StreamEvent =
  | { type: "token"; text: string }
  | { type: "done"; message: ChatMessage }
  | { type: "error"; message: string };

/** Callbacks driven by the parsed event stream. */
export interface StreamHandlers {
  /** Called for each `token` delta — append `text` to the live bubble. */
  onToken: (text: string) => void;
  /** Called once with the persisted assistant message; the stream then ends. */
  onDone: (message: ChatMessage) => void;
  /** Called on a server `error` frame or a transport/parse failure. */
  onError: (message: string) => void;
}

/**
 * Parse one SSE frame's worth of `data:` lines into a [StreamEvent].
 *
 * Thin chat-typed wrapper over the transport-generic {@link parseSseDataFrame}.
 * Exported for unit testing and reused by {@link SseParser}.
 */
export function parseSseFrame(frame: string): StreamEvent | null {
  return parseSseDataFrame<StreamEvent>(frame);
}

/**
 * A stateful SSE frame splitter specialized to the chat `StreamEvent` shape.
 *
 * Now a thin wrapper over the shared {@link SseFrameSplitter}; kept as a named
 * export so existing callers/tests are unaffected.
 */
export class SseParser extends SseFrameSplitter<StreamEvent> {
  constructor() {
    super(parseSseFrame);
  }
}

/**
 * POST a user message to a chat and consume the `text/event-stream` response.
 *
 * Uses raw `fetch()` + `response.body.getReader()` (not `EventSource`, which
 * can't POST a body). Credentials/header handling mirrors `request()` in
 * `api.ts`: that helper relies on fetch's default same-origin credentials
 * (cookies are sent for same-origin requests automatically) and sets explicit
 * `Content-Type`/`Accept` headers — we do the same, only with an
 * `Accept: text/event-stream`.
 *
 * Resolves when the stream ends (a `done` or `error` frame, or EOF). Throws
 * `ApiError` for a non-OK response so callers can branch on status.
 *
 * @param model Optional model slug forwarded in the POST body. When omitted
 *   the server uses its configured default.
 */
export async function streamChatMessage(
  chatId: string,
  content: string,
  handlers: StreamHandlers,
  model?: string,
): Promise<void> {
  let res: Response;
  const requestBody: Record<string, unknown> = { content };
  if (model) requestBody.model = model;
  try {
    res = await fetch(`/api/chats/${enc(chatId)}/messages`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "text/event-stream",
      },
      body: JSON.stringify(requestBody),
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : "network error";
    handlers.onError(message);
    return;
  }

  if (!res.ok || !res.body) {
    throw await errorFromResponse(res);
  }

  const parser = new SseParser();

  const dispatch = (evt: StreamEvent): boolean => {
    switch (evt.type) {
      case "token":
        handlers.onToken(evt.text);
        return false;
      case "done":
        handlers.onDone(evt.message);
        return true; // terminal
      case "error":
        handlers.onError(evt.message);
        return true; // terminal
    }
  };

  try {
    await consumeSseFrames(res.body, parser, (evt) =>
      dispatch(evt) ? true : null,
    );
  } catch (err) {
    const message = err instanceof Error ? err.message : "stream read error";
    handlers.onError(message);
  }
}
