import type { Terminal } from "@xterm/xterm";

/** Keep a reader attached to retained text across an authoritative redraw. */
export function captureTerminalViewport(term: Terminal) {
  const buffer = term.buffer.active;
  if (buffer.viewportY >= buffer.baseY) return null;
  return {
    row: buffer.viewportY,
    text: buffer.getLine(buffer.viewportY)?.translateToString(true) ?? "",
  };
}

export function restoreTerminalViewport(
  term: Terminal,
  anchor: ReturnType<typeof captureTerminalViewport>,
) {
  if (!anchor) return;
  const buffer = term.buffer.active;
  const matches = (row: number) =>
    buffer.getLine(row)?.translateToString(true) === anchor.text;
  if (matches(anchor.row)) {
    term.scrollToLine(anchor.row);
    return;
  }
  // Retention eviction or a width change can move the anchor. Prefer the
  // nearest matching row so repeated text does not jump to an unrelated turn.
  for (let distance = 1; distance < buffer.length; distance++) {
    for (const row of [anchor.row - distance, anchor.row + distance]) {
      if (row >= 0 && row < buffer.length && matches(row)) {
        term.scrollToLine(row);
        return;
      }
    }
  }
  term.scrollToLine(Math.min(anchor.row, buffer.baseY));
}
