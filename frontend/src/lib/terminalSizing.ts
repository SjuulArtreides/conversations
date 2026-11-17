/**
 * Pure sizing math for the "Fit columns and rows to screen" action.
 *
 * The browser is a mirror of the source PTY grid; scaling the render (font size)
 * only changes how big that grid looks. To make the source actually report a
 * grid that fits the viewer's screen at a readable font, we compute a target
 * `cols × rows` here and POST it to the CLI host (`POST /api/live/{id}/resize`).
 *
 * The math is a similar-triangles projection: from the terminal's CURRENT cell
 * size (measured off the rendered element) and font, scale each cell to the
 * TARGET font, then divide the available viewport by the target cell size. Kept
 * side-effect-free (no DOM / `window` reads) so it is directly unit-testable —
 * the component measures the DOM and passes plain numbers in.
 */

/** Clamp bounds for a browser-proposed grid — structural guard-rails so a bad
 *  measurement (or a hostile client) can never ask for a degenerate or absurd
 *  PTY size. These upper bounds match the server and wrapper protocol limit of 1000. */
export const MIN_RESIZE_COLS = 36;
export const MIN_RESIZE_ROWS = 10;
export const MAX_RESIZE_COLS = 1000;
export const MAX_RESIZE_ROWS = 1000;

/** Floor `value` into `[min, max]`, rounding down first. */
export function clampInt(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, Math.floor(value)));
}

export interface ProposeResizeGridParams {
  /** Current source grid the terminal reports. */
  cols: number;
  rows: number;
  /** Pixel size of the rendered terminal element at `currentFontPx`. */
  renderedWidth: number;
  renderedHeight: number;
  /** Font the rendered measurements were taken at. */
  currentFontPx: number;
  /** Viewport the resized grid should fill, in CSS pixels. */
  availableWidth: number;
  availableHeight: number;
  /** Font we want the resized grid to render at. */
  targetFontPx: number;
}

/**
 * Propose a `cols × rows` grid that fills `availableWidth × availableHeight` at
 * `targetFontPx`, derived from the current rendered cell size. Returns `null`
 * when any input is non-positive (dimensions not measurable yet), so the caller
 * can surface "not ready" rather than posting a garbage size. The result is
 * clamped into the `MIN/MAX_RESIZE_*` bounds.
 */
export function proposeResizeGrid(
  params: ProposeResizeGridParams,
): { cols: number; rows: number } | null {
  const {
    cols,
    rows,
    renderedWidth,
    renderedHeight,
    currentFontPx,
    availableWidth,
    availableHeight,
    targetFontPx,
  } = params;
  if (
    cols <= 0 ||
    rows <= 0 ||
    renderedWidth <= 0 ||
    renderedHeight <= 0 ||
    currentFontPx <= 0 ||
    availableWidth <= 0 ||
    availableHeight <= 0 ||
    targetFontPx <= 0
  ) {
    return null;
  }

  const scale = targetFontPx / currentFontPx;
  const cellWidth = (renderedWidth / cols) * scale;
  const cellHeight = (renderedHeight / rows) * scale;
  if (cellWidth <= 0 || cellHeight <= 0) return null;

  return {
    cols: clampInt(
      availableWidth / cellWidth,
      MIN_RESIZE_COLS,
      MAX_RESIZE_COLS,
    ),
    rows: clampInt(
      availableHeight / cellHeight,
      MIN_RESIZE_ROWS,
      MAX_RESIZE_ROWS,
    ),
  };
}
