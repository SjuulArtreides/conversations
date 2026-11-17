/** A diagram drawn in the terminal: its first node row, last row and right edge. */
export interface DiagramAnchor {
  row: number;
  bottom: number;
  col: number;
}

// Box-drawing (U+2500–U+257F) plus the arrowheads diagram renderers draw.
const DIAGRAM_GLYPH = /[─-╿▲▴▶▸►▼▾◀◂◄]/;
// A node's top edge. A table header (`┌──┬──┐`) never matches: `┬` breaks it.
const BOX_TOP = /[┌╭]─+[┐╮]/g;
// An edge between nodes: an arrowhead, or a line leaving a box's side or corner.
const CONNECTOR = /[▲▴▶▸►▼▾◀◂◄]|[│┤┘┐]─|─[│├└┌]/;
// Rows without glyphs a diagram may contain (e.g. a label between nodes).
const MAX_GAP = 2;
// One box is a prompt or dialog frame; a diagram connects several nodes.
const MIN_BOXES = 2;

/**
 * Find diagrams an agent's TUI drew in terminal rows. Each line must hold one
 * character per terminal cell. Stacked frames without edges between them (a
 * status panel above a prompt box) are not diagrams.
 */
export function findDiagramAnchors(lines: readonly string[]): DiagramAnchor[] {
  const anchors: DiagramAnchor[] = [];
  let boxes = 0;
  let connected = false;
  let top = -1;
  let bottom = -1;
  let right = 0;
  let gap = 0;
  const close = () => {
    if (boxes >= MIN_BOXES && connected)
      anchors.push({ row: top, bottom, col: right });
    boxes = 0;
    connected = false;
    top = -1;
    bottom = -1;
    right = 0;
    gap = 0;
  };
  lines.forEach((line, row) => {
    if (!DIAGRAM_GLYPH.test(line)) {
      if (top < 0 || ++gap > MAX_GAP) close();
      return;
    }
    gap = 0;
    const tops = line.match(BOX_TOP)?.length ?? 0;
    if (tops && top < 0) top = row;
    boxes += tops;
    connected ||= CONNECTOR.test(line);
    bottom = row;
    for (let col = line.length - 1; col >= 0; col--) {
      if (DIAGRAM_GLYPH.test(line[col])) {
        right = Math.max(right, col + 1);
        break;
      }
    }
  });
  close();
  return anchors;
}
