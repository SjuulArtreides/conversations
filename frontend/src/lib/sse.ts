/**
 * Transport-generic Server-Sent-Events helpers, shared by every SSE consumer
 * in the app (the chat stream and the live-session stream).
 *
 * These deliberately know nothing about the *shape* of a frame's payload — each
 * consumer owns its own decoded-event type and supplies a `parse` callback. The
 * chat stream's `StreamEvent` union and the live view's `LiveFrame` union are
 * thus fully independent; widening one can never break the other.
 */

/**
 * Concatenate one SSE frame's `data:` lines and `JSON.parse` the result.
 *
 * A frame may carry multiple `data:` lines; per the SSE spec their payloads are
 * joined with a newline before interpretation. CRLF endings and an optional
 * single space after the colon are tolerated. Returns `null` for frames with no
 * data lines (a stray comment / keep-alive) or a whitespace-only payload — the
 * caller decides what an absent payload means.
 *
 * The generic parameter lets the caller assert the decoded shape; the JSON is
 * trusted to match the server contract (same as the chat stream did inline).
 */
export function parseSseDataFrame<T>(frame: string): T | null {
  const dataLines: string[] = [];
  for (const rawLine of frame.split("\n")) {
    const line = rawLine.replace(/\r$/, "");
    if (line.startsWith("data:")) {
      dataLines.push(line.slice(5).replace(/^ /, ""));
    }
  }
  if (dataLines.length === 0) return null;
  const payload = dataLines.join("\n");
  if (!payload.trim()) return null;
  return JSON.parse(payload) as T;
}

/**
 * A stateful SSE frame splitter. Feed it decoded text chunks via `push`; it
 * buffers across calls and returns every complete decoded frame found so far,
 * holding any trailing partial frame until the rest arrives (so a frame split
 * across two network reads is handled). `flush` yields a final frame that lacks
 * a trailing blank line.
 *
 * The `parse` callback maps one raw frame string to a decoded value (or `null`
 * to skip it) — pass `parseSseDataFrame<YourFrame>` for the common JSON case.
 */
export class SseFrameSplitter<T> {
  private buffer = "";

  constructor(private readonly parse: (frame: string) => T | null) {}

  /** Feed a decoded text chunk; return every complete frame it completes. */
  push(chunk: string): T[] {
    this.buffer += chunk;
    // Frames are separated by a blank line. Normalize CRLF first so the
    // "\n\n" split works regardless of the server's line endings.
    this.buffer = this.buffer.replace(/\r\n/g, "\n");
    const out: T[] = [];
    let sep: number;
    while ((sep = this.buffer.indexOf("\n\n")) !== -1) {
      const frame = this.buffer.slice(0, sep);
      this.buffer = this.buffer.slice(sep + 2);
      const value = this.parse(frame);
      if (value != null) out.push(value);
    }
    return out;
  }

  /** Flush any buffered final frame (one with no trailing blank line). */
  flush(): T[] {
    const rest = this.buffer;
    this.buffer = "";
    if (!rest.trim()) return [];
    const value = this.parse(rest);
    return value != null ? [value] : [];
  }
}

/** Consume frames through EOF or the first non-null dispatch result. */
export async function consumeSseFrames<T, R>(
  body: ReadableStream<Uint8Array>,
  splitter: SseFrameSplitter<T>,
  dispatch: (frame: T) => R | null,
): Promise<R | null> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      for (const frame of splitter.push(
        decoder.decode(value, { stream: true }),
      )) {
        const result = dispatch(frame);
        if (result !== null) return result;
      }
    }
    for (const frame of splitter.flush()) {
      const result = dispatch(frame);
      if (result !== null) return result;
    }
    return null;
  } finally {
    reader.releaseLock();
  }
}
