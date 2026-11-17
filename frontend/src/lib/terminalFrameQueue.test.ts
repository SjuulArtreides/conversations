import { describe, expect, it } from "vitest";
import { createTerminalFrameQueue } from "./terminalFrameQueue";

describe("terminal replay batching", () => {
  it("loads a queued history burst without one asynchronous write per network frame", () => {
    const queue = createTerminalFrameQueue();
    const received: string[] = [];
    const complete: (() => void)[] = [];
    const writer = (
      frame: { bytes: Uint8Array },
      _count: number,
      done: () => void,
    ) => {
      received.push(new TextDecoder().decode(frame.bytes));
      complete.push(done);
    };
    for (let i = 0; i < 1000; i++) {
      queue.frame(
        { cols: 80, rows: 24, bytes: new TextEncoder().encode(`${i}\r\n`) },
        writer,
      );
    }
    expect(received).toEqual(["0\r\n"]);
    complete.shift()!();
    expect(received).toHaveLength(2);
    expect(received.join("")).toBe(
      Array.from({ length: 1000 }, (_, i) => `${i}\r\n`).join(""),
    );
  });

  it("never coalesces bytes across a resize or reset", () => {
    const queue = createTerminalFrameQueue();
    const events: string[] = [];
    const complete: (() => void)[] = [];
    const writer = (
      frame: { cols: number; bytes: Uint8Array },
      _count: number,
      done: () => void,
    ) => {
      events.push(`${frame.cols}:${new TextDecoder().decode(frame.bytes)}`);
      complete.push(done);
    };
    const send = (cols: number, text: string) =>
      queue.frame(
        { cols, rows: 24, bytes: new TextEncoder().encode(text) },
        writer,
      );
    send(80, "first");
    send(80, "second");
    send(40, "narrow");
    queue.enqueue((done) => {
      events.push("reset");
      done();
    });
    send(40, "after");
    while (complete.length) complete.shift()!();
    expect(events).toEqual([
      "80:first",
      "80:second",
      "40:narrow",
      "reset",
      "40:after",
    ]);
  });
});
