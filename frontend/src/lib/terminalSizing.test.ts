import { describe, expect, it } from "vitest";
import {
  clampInt,
  MIN_RESIZE_COLS,
  MIN_RESIZE_ROWS,
  proposeResizeGrid,
} from "./terminalSizing";

describe("clampInt", () => {
  it("floors before clamping", () => {
    expect(clampInt(41.9, 0, 100)).toBe(41);
  });

  it("clamps below the floor up and above the ceiling down", () => {
    expect(clampInt(2, 10, 100)).toBe(10);
    expect(clampInt(999, 10, 100)).toBe(100);
  });
});

describe("proposeResizeGrid", () => {
  // A square 80x24 grid rendered as 640x384 px at 13px font ⇒ 8x16 px cells.
  const base = {
    cols: 80,
    rows: 24,
    renderedWidth: 640,
    renderedHeight: 384,
    currentFontPx: 13,
  };

  it("projects the current cell size to a bigger viewport at the same font", () => {
    // Same font ⇒ cells stay 8x16; a 1600x800 viewport ⇒ 200 cols, 50 rows.
    const grid = proposeResizeGrid({
      ...base,
      currentFontPx: 13,
      targetFontPx: 13,
      availableWidth: 1600,
      availableHeight: 800,
    });
    expect(grid).toEqual({ cols: 200, rows: 50 });
  });

  it("uses a wide and tall terminal viewport beyond the old 240 by 100 cap", () => {
    expect(
      proposeResizeGrid({
        ...base,
        targetFontPx: 13,
        availableWidth: 2276,
        availableHeight: 1800,
      }),
    ).toEqual({ cols: 284, rows: 112 });
  });

  it("scales the cell size with the target font (bigger font ⇒ fewer cells)", () => {
    // Doubling the font doubles each cell (16x32), halving the fit vs. above.
    const grid = proposeResizeGrid({
      ...base,
      currentFontPx: 13,
      targetFontPx: 26,
      availableWidth: 1600,
      availableHeight: 800,
    });
    expect(grid).toEqual({ cols: 100, rows: 25 });
  });

  it("clamps a tiny viewport up to the minimum grid", () => {
    const grid = proposeResizeGrid({
      ...base,
      targetFontPx: 13,
      availableWidth: 40,
      availableHeight: 40,
    });
    expect(grid).toEqual({ cols: MIN_RESIZE_COLS, rows: MIN_RESIZE_ROWS });
  });

  it("clamps a huge viewport down to the maximum grid", () => {
    const grid = proposeResizeGrid({
      ...base,
      targetFontPx: 13,
      availableWidth: 100_000,
      availableHeight: 100_000,
    });
    expect(grid).toEqual({ cols: 1000, rows: 1000 });
  });

  it.each([
    ["cols", { cols: 0 }],
    ["rows", { rows: 0 }],
    ["renderedWidth", { renderedWidth: 0 }],
    ["renderedHeight", { renderedHeight: 0 }],
    ["currentFontPx", { currentFontPx: 0 }],
    ["availableWidth", { availableWidth: 0 }],
    ["availableHeight", { availableHeight: 0 }],
    ["targetFontPx", { targetFontPx: 0 }],
  ])("returns null when %s is not measurable", (_label, override) => {
    expect(
      proposeResizeGrid({
        ...base,
        targetFontPx: 13,
        availableWidth: 800,
        availableHeight: 400,
        ...override,
      }),
    ).toBeNull();
  });
});
