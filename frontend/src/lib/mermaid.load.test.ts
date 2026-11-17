import { beforeEach, describe, expect, it, vi } from "vitest";

const mermaidApi = vi.hoisted(() => ({
  initialize: vi.fn(),
  parse: vi.fn(),
  render: vi.fn(),
}));
const chunk = vi.hoisted(() => ({ failNextImport: false }));

vi.mock("mermaid", () => {
  if (chunk.failNextImport) {
    chunk.failNextImport = false;
    throw new TypeError("Failed to fetch dynamically imported module");
  }
  return { default: mermaidApi };
});

const SOURCE = "flowchart LR\nA-->B";
const SVG = "<svg><text>ok</text></svg>";

describe("renderMermaidSvg loading", () => {
  beforeEach(() => {
    vi.resetModules();
    chunk.failNextImport = false;
    mermaidApi.initialize.mockReset();
    mermaidApi.parse
      .mockReset()
      .mockResolvedValue({ diagramType: "flowchart" });
    mermaidApi.render.mockReset().mockResolvedValue({ svg: SVG });
  });

  it("imports the Mermaid chunk again after a failed import and reports it as a load failure", async () => {
    chunk.failNextImport = true;
    const { renderMermaidSvg, MermaidLoadError } = await import("./mermaid");

    const failed = renderMermaidSvg("m1", SOURCE);
    // vitest wraps a throwing mock factory in its own error, so assert the
    // classification rather than the browser's message text.
    await expect(failed).rejects.toBeInstanceOf(MermaidLoadError);
    await expect(renderMermaidSvg("m2", SOURCE)).resolves.toBe(SVG);
    expect(mermaidApi.initialize).toHaveBeenCalledTimes(1);
  });

  it("initializes Mermaid again after a failed initialization", async () => {
    mermaidApi.initialize.mockImplementationOnce(() => {
      throw new Error("initialize exploded");
    });
    const { renderMermaidSvg, MermaidLoadError } = await import("./mermaid");

    const failed = renderMermaidSvg("m1", SOURCE);
    await expect(failed).rejects.toThrow("initialize exploded");
    await expect(failed).rejects.not.toBeInstanceOf(MermaidLoadError);
    await expect(renderMermaidSvg("m2", SOURCE)).resolves.toBe(SVG);
    expect(mermaidApi.initialize).toHaveBeenCalledTimes(2);
  });

  it("keeps strict security for every render", async () => {
    const { renderMermaidSvg } = await import("./mermaid");
    await renderMermaidSvg("m3", SOURCE);

    expect(mermaidApi.initialize).toHaveBeenCalledWith(
      expect.objectContaining({ securityLevel: "strict", startOnLoad: false }),
    );
  });

  it("surfaces the parser diagnostic and does not let it block later diagrams", async () => {
    mermaidApi.parse.mockRejectedValueOnce(
      new Error("Parse error on line 2:\nA--\n---^\nExpecting 'LINK'"),
    );
    const { renderMermaidSvg, MermaidLoadError } = await import("./mermaid");

    const failed = renderMermaidSvg("bad", "flowchart LR\nA--");
    await expect(failed).rejects.toThrow("Parse error on line 2");
    await expect(failed).rejects.not.toBeInstanceOf(MermaidLoadError);
    await expect(renderMermaidSvg("good", SOURCE)).resolves.toBe(SVG);
    expect(mermaidApi.render).toHaveBeenCalledTimes(1);
  });
});
