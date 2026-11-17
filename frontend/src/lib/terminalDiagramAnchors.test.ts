import { describe, expect, it } from "vitest";
import { findDiagramAnchors } from "./terminalDiagramAnchors";

describe("findDiagramAnchors", () => {
  it("anchors a rendered flowchart at its first node and right edge", () => {
    const lines = [
      "• Here is the flow:",
      "",
      "  ┌──────────────┐",
      "  │ BYOC release │──Copies files──┐",
      "  └──────────────┘                │",
      "                                  │",
      "  ┌──────────────┐                │",
      "  │ Archive      │◄───────────────┘",
      "  └──────────────┘",
      "",
      "Changing the BYOC copy alone doesn't change the archive.",
    ];

    expect(findDiagramAnchors(lines)).toEqual([{ row: 2, bottom: 8, col: 35 }]);
  });

  it("keeps a diagram whole across a short label row without glyphs", () => {
    const lines = ["┌───┐", "└─┬─┘", "  copies", "  ▼", "┌───┐", "└───┘"];

    expect(findDiagramAnchors(lines)).toEqual([{ row: 0, bottom: 5, col: 5 }]);
  });

  it("finds separate diagrams split by prose", () => {
    const diagram = ["┌──┐ ┌──┐", "│  │─►│  │", "└──┘ └──┘"];
    const lines = [...diagram, "", "", "", "Then:", ...diagram];

    expect(findDiagramAnchors(lines)).toEqual([
      { row: 0, bottom: 2, col: 10 },
      { row: 7, bottom: 9, col: 10 },
    ]);
  });

  it("ignores a lone prompt or dialog frame", () => {
    const lines = [
      "╭────────────────╮",
      "│ > ask anything │",
      "╰────────────────╯",
    ];

    expect(findDiagramAnchors(lines)).toEqual([]);
  });

  it("ignores stacked frames with no edge between them", () => {
    const lines = [
      "╭────────────────╮",
      "│ model: gpt     │",
      "╰────────────────╯",
      "",
      "╭────────────────╮",
      "│ > ask anything │",
      "╰────────────────╯",
    ];

    expect(findDiagramAnchors(lines)).toEqual([]);
  });

  it("ignores markdown tables", () => {
    const lines = [
      "┌──────┬───────┐",
      "│ Name │ Value │",
      "├──────┼───────┤",
      "│ a    │ 1     │",
      "└──────┴───────┘",
      "┌──────┬───────┐",
      "│ b    │ 2     │",
      "└──────┴───────┘",
    ];

    expect(findDiagramAnchors(lines)).toEqual([]);
  });
});
