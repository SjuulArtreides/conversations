import type { IBufferLine, Terminal } from "@xterm/xterm";
import * as React from "react";
import { findDiagramAnchors } from "./terminalDiagramAnchors";

/** A diagram icon's position in `container` pixels. */
export interface DiagramMarker {
  key: string;
  left: number;
  top: number;
}

/** The marker button's edge length in pixels (Tailwind `h-6 w-6`). */
export const DIAGRAM_MARKER_PX = 24;

/**
 * Place a marker beside each diagram drawn in the terminal's visible rows.
 * Rescans after every xterm render (output, scroll, resize), so markers follow
 * the diagram. `terminalKey` must change whenever `getTerminal` returns a
 * different terminal (session switch, entering or leaving the reading view).
 */
export function useTerminalDiagramMarkers(
  getTerminal: () => Terminal | null,
  container: React.RefObject<HTMLElement | null>,
  enabled: boolean,
  terminalKey: unknown,
): DiagramMarker[] {
  const [markers, setMarkers] = React.useState<DiagramMarker[]>([]);
  const getTerminalRef = React.useRef(getTerminal);
  getTerminalRef.current = getTerminal;

  React.useEffect(() => {
    const term = getTerminalRef.current();
    if (!enabled || !term || typeof term.onRender !== "function") {
      setMarkers([]);
      return;
    }
    let frame = 0;
    const scan = () => {
      frame = 0;
      const screen = term.element?.querySelector(".xterm-screen");
      const box = container.current;
      if (!screen || !box || !term.cols || !term.rows) return;
      const buffer = term.buffer.active;
      // Also read a screen above the viewport, so a diagram whose first node
      // has scrolled out of view keeps its marker.
      const first = Math.max(0, buffer.viewportY - term.rows);
      const lines = Array.from(
        { length: buffer.viewportY + term.rows - first },
        (_, index) => cellText(buffer.getLine(first + index), term.cols),
      );
      const grid = screen.getBoundingClientRect();
      const origin = box.getBoundingClientRect();
      const cellWidth = grid.width / term.cols;
      const cellHeight = grid.height / term.rows;
      const maxLeft = box.clientWidth - DIAGRAM_MARKER_PX;
      const next = findDiagramAnchors(lines)
        .filter(({ bottom }) => first + bottom >= buffer.viewportY)
        .map(({ row, col }) => ({
          key: `${first + row}`,
          left: Math.max(
            0,
            Math.min(grid.left - origin.left + (col + 1) * cellWidth, maxLeft),
          ),
          top:
            grid.top -
            origin.top +
            Math.max(0, first + row - buffer.viewportY) * cellHeight,
        }));
      setMarkers((previous) =>
        JSON.stringify(previous) === JSON.stringify(next) ? previous : next,
      );
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(scan);
    };
    const render = term.onRender(schedule);
    schedule();
    return () => {
      render.dispose();
      if (frame) cancelAnimationFrame(frame);
      setMarkers([]);
    };
  }, [container, enabled, terminalKey]);

  return markers;
}

/**
 * One character per terminal cell, so a string index is a column. A wide
 * character fills its first cell; its continuation cell is a space.
 */
function cellText(line: IBufferLine | undefined, cols: number): string {
  if (!line) return "";
  if (typeof line.getCell !== "function") return line.translateToString(true);
  let text = "";
  for (let col = 0; col < cols; col++) {
    const cell = line.getCell(col);
    const chars = cell?.getChars() ?? "";
    text += chars.length === 1 ? chars : chars ? "█" : " ";
  }
  return text.trimEnd();
}
