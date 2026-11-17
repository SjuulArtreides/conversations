import { describe, expect, it } from "vitest";
import { Terminal } from "@xterm/xterm";
import { createSerialOperationQueue } from "./serialOperationQueue";

function line(term: Terminal, index: number) {
  return term.buffer.active.getLine(index)?.translateToString(true);
}

describe("serial terminal operations", () => {
  it("keeps each real xterm write at the frame geometry it arrived with", async () => {
    const term = new Terminal({ cols: 10, rows: 3, scrollback: 20 });
    const queue = createSerialOperationQueue();

    const frame = (cols: number, bytes: string) =>
      queue.enqueue((done) => {
        term.resize(cols, 3);
        term.write(bytes, done);
      });

    frame(10, "\x1b[1;10HX");
    frame(5, "Y");
    await new Promise<void>((resolve) =>
      queue.enqueue((done) => {
        resolve();
        done();
      }),
    );

    expect(line(term, 0)).toBe("    Y");
    expect(line(term, 1)).toBe("");
    expect(term.buffer.active.cursorX).toBe(5);
    expect(term.buffer.active.cursorY).toBe(0);
    queue.dispose();
    term.dispose();
  });
});
