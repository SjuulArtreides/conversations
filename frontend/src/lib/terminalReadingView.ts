import type { Terminal } from "@xterm/xterm";

/** A bounded, immutable reading buffer while the live terminal keeps parsing. */
export function createTerminalReadingView(
  live: Terminal,
  serialize: () => string,
  create: () => Terminal,
  changed: (reader: Terminal | null) => void,
) {
  let reader: Terminal | null = null;
  let ready = false;
  let disposed = false;
  let waiting: (() => void)[] = [];
  let scroll: { dispose(): void } | undefined;

  const resume = () => {
    const old = reader;
    reader = null;
    ready = false;
    scroll?.dispose();
    scroll = undefined;
    old?.dispose();
    if (!disposed) {
      live.scrollToBottom();
      changed(null);
    }
    const callbacks = waiting;
    waiting = [];
    callbacks.forEach((done) => done());
  };

  return {
    /** Call before each live buffer mutation, including resets and resizes. */
    beforeWrite(done: () => void) {
      if (disposed) return;
      if (reader) {
        if (ready) done();
        else waiting.push(done);
        return;
      }
      const buffer = live.buffer.active;
      if (buffer.type === "alternate" || buffer.viewportY >= buffer.baseY) {
        done();
        return;
      }
      const row = buffer.viewportY;
      const bytes = serialize();
      const snapshot = create();
      reader = snapshot;
      waiting.push(done);
      snapshot.write(bytes, () => {
        if (disposed || reader !== snapshot) return;
        snapshot.scrollToLine(row);
        ready = true;
        changed(snapshot);
        scroll = snapshot.onScroll(() => {
          if (snapshot.buffer.active.viewportY >= snapshot.buffer.active.baseY)
            resume();
        });
        const callbacks = waiting;
        waiting = [];
        callbacks.forEach((callback) => callback());
      });
    },
    resume,
    dispose() {
      disposed = true;
      waiting = [];
      resume();
    },
  };
}
