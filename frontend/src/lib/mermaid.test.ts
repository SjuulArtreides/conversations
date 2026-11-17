import { describe, expect, it } from "vitest";
import {
  looksLikeMermaid,
  mermaidAspectRatio,
  mermaidFrameDocument,
} from "./mermaid";

describe("looksLikeMermaid", () => {
  it("accepts an explicit Mermaid fence regardless of source validity", () => {
    expect(looksLikeMermaid("not finished yet", "mermaid")).toBe(true);
  });

  it("detects a clearly unlabeled Mermaid definition", () => {
    expect(looksLikeMermaid("flowchart LR\n  A --> B")).toBe(true);
    expect(
      looksLikeMermaid("---\ntitle: Login\n---\nsequenceDiagram\nA->>B: Hi"),
    ).toBe(true);
  });

  it("does not reinterpret ordinary or differently labeled code", () => {
    expect(looksLikeMermaid("const graph = buildGraph()", "")).toBe(false);
    expect(looksLikeMermaid("graph LR\nA-->B", "text")).toBe(false);
  });
});

describe("Mermaid frame", () => {
  it("places SVG in a network-isolated document", () => {
    const doc = mermaidFrameDocument('<svg viewBox="0 0 400 200"></svg>');
    expect(doc).toContain("default-src 'none'");
    expect(doc).toContain('<svg viewBox="0 0 400 200"></svg>');
  });

  it("reads and bounds the SVG aspect ratio", () => {
    expect(mermaidAspectRatio('<svg viewBox="0 0 400 200"></svg>')).toBe(2);
    expect(mermaidAspectRatio('<svg viewBox="0 0 9999 10"></svg>')).toBe(4);
    expect(mermaidAspectRatio("<svg></svg>")).toBeCloseTo(16 / 9);
  });
});

describe("renderMermaidSvg with the real Mermaid parser", () => {
  it("reports Mermaid's own syntax diagnostic for a malformed diagram", async () => {
    const { renderMermaidSvg } = await import("./mermaid");
    await expect(
      renderMermaidSvg("real-bad", "flowchart LR\n  A --> \n  (((\n"),
    ).rejects.toThrow(/Parse error on line \d+/);
  }, 20000);
});
