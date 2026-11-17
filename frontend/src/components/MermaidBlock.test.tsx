import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MermaidLoadError, renderMermaidSvg } from "@/lib/mermaid";
import { MermaidBlock } from "./MermaidBlock";

vi.mock("@/lib/mermaid", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/mermaid")>("@/lib/mermaid");
  return { ...actual, renderMermaidSvg: vi.fn() };
});

const renderSvg = vi.mocked(renderMermaidSvg);
const SOURCE = "flowchart LR\n  A[Copy] --> B[View]";
const PREVIEW = "Preview Mermaid diagram";

describe("MermaidBlock", () => {
  beforeEach(() => {
    renderSvg.mockReset();
    renderSvg.mockResolvedValue(
      '<svg viewBox="0 0 400 200"><text>diagram</text></svg>',
    );
  });

  it("copies the exact source without converting it to a table", async () => {
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);

    await user.click(screen.getByRole("button", { name: "Copy code" }));
    await expect(navigator.clipboard.readText()).resolves.toBe(SOURCE);
  });

  it("renders nothing until the diagram icon is pressed", () => {
    render(<MermaidBlock source={SOURCE} />);

    const toggle = screen.getByRole("button", { name: PREVIEW });
    expect(toggle).toHaveAttribute("aria-pressed", "false");
    expect(toggle.querySelector("svg")).not.toBeNull();
    expect(renderSvg).not.toHaveBeenCalled();
  });

  it("toggles between source and an isolated rendered diagram with the icon", async () => {
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);

    await user.click(screen.getByRole("button", { name: PREVIEW }));

    const frame = await screen.findByTitle("Mermaid diagram");
    expect(renderSvg).toHaveBeenCalledWith(
      expect.stringMatching(/^mermaid-/),
      SOURCE,
    );
    expect(frame).toHaveAttribute("sandbox", "");
    expect(frame.getAttribute("srcdoc")).toContain("<text>diagram</text>");
    expect(screen.getByRole("button", { name: PREVIEW })).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    await user.click(screen.getByRole("button", { name: PREVIEW }));
    expect(screen.queryByTitle("Mermaid diagram")).not.toBeInTheDocument();
    expect(
      screen.getByText("A[Copy] --> B[View]", { exact: false }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: PREVIEW })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
  });

  it("shows a malformed diagram's error and keeps the source one toggle away", async () => {
    renderSvg.mockRejectedValue(new Error("The Mermaid source is not valid."));
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);

    await user.click(screen.getByRole("button", { name: PREVIEW }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The Mermaid source is not valid.",
    );

    await user.click(screen.getByRole("button", { name: PREVIEW }));
    expect(
      screen.getByText("A[Copy] --> B[View]", { exact: false }),
    ).toBeInTheDocument();
  });

  it("retries a failed render and shows the diagram once it succeeds", async () => {
    renderSvg.mockRejectedValueOnce(new Error("Failed to fetch chunk"));
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);

    await user.click(screen.getByRole("button", { name: PREVIEW }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Failed to fetch chunk",
    );

    await user.click(screen.getByRole("button", { name: "Retry" }));

    const frame = await screen.findByTitle("Mermaid diagram");
    expect(frame.getAttribute("srcdoc")).toContain("<text>diagram</text>");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(renderSvg).toHaveBeenCalledTimes(2);
  });

  it("offers a page reload, besides Retry, when the Mermaid bundle cannot load", async () => {
    const reload = vi.fn();
    const realLocation = window.location;
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...realLocation, reload },
    });
    try {
      renderSvg.mockRejectedValueOnce(
        new MermaidLoadError("Failed to fetch dynamically imported module"),
      );
      const user = userEvent.setup();
      render(<MermaidBlock source={SOURCE} />);

      await user.click(screen.getByRole("button", { name: PREVIEW }));
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "Failed to fetch dynamically imported module",
      );
      expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: "Reload page" }));
      expect(reload).toHaveBeenCalledTimes(1);
    } finally {
      Object.defineProperty(window, "location", {
        configurable: true,
        value: realLocation,
      });
    }
  });

  it("does not suggest a page reload for a malformed diagram", async () => {
    renderSvg.mockRejectedValue(new Error("Parse error on line 2"));
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);

    await user.click(screen.getByRole("button", { name: PREVIEW }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Parse error on line 2",
    );
    expect(
      screen.queryByRole("button", { name: "Reload page" }),
    ).not.toBeInTheDocument();
  });

  it("drops a stale diagram when streaming changes the source", async () => {
    let resolveFirst!: (svg: string) => void;
    renderSvg.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const user = userEvent.setup();
    const { rerender } = render(<MermaidBlock source={SOURCE} />);
    await user.click(screen.getByRole("button", { name: PREVIEW }));

    const nextSource = `${SOURCE}\n  B --> C[Done]`;
    rerender(<MermaidBlock source={nextSource} />);
    resolveFirst('<svg viewBox="0 0 100 100"><text>stale</text></svg>');

    await waitFor(() => {
      expect(screen.getByRole("button", { name: PREVIEW })).toHaveAttribute(
        "aria-pressed",
        "false",
      );
    });
    expect(screen.queryByTitle("Mermaid diagram")).not.toBeInTheDocument();
    expect(
      screen.getByText("B --> C[Done]", { exact: false }),
    ).toBeInTheDocument();
  });

  it("expands a wide diagram without rendering again, supports zoom, and restores focus on Escape", async () => {
    renderSvg.mockResolvedValue(
      '<svg viewBox="0 0 2400 400"><text>wide diagram</text></svg>',
    );
    const user = userEvent.setup();
    render(<MermaidBlock source={SOURCE} />);
    await user.click(screen.getByRole("button", { name: PREVIEW }));
    const expand = await screen.findByRole("button", {
      name: "Expand Mermaid diagram",
    });
    await user.click(expand);
    expect(
      screen.getByRole("dialog", { name: "Mermaid diagram" }),
    ).toBeInTheDocument();
    const frame = screen.getByTitle("Expanded Mermaid diagram");
    expect(frame).toHaveAttribute("sandbox", "");
    await user.click(screen.getByRole("button", { name: "Actual size" }));
    expect(frame).toHaveStyle({ width: "2400px", height: "400px" });
    await user.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(frame).toHaveStyle({ width: "3000px", height: "500px" });
    expect(renderSvg).toHaveBeenCalledTimes(1);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(expand).toHaveFocus();
  });

  it("fits the diagram when the popup viewport first mounts", async () => {
    const width = vi
      .spyOn(HTMLElement.prototype, "clientWidth", "get")
      .mockReturnValue(1000);
    const height = vi
      .spyOn(HTMLElement.prototype, "clientHeight", "get")
      .mockReturnValue(600);
    try {
      renderSvg.mockResolvedValue('<svg viewBox="0 0 2400 400"></svg>');
      const user = userEvent.setup();
      render(<MermaidBlock source={SOURCE} />);
      await user.click(screen.getByRole("button", { name: PREVIEW }));
      await user.click(
        await screen.findByRole("button", { name: "Expand Mermaid diagram" }),
      );
      expect(screen.getByTitle("Expanded Mermaid diagram")).toHaveStyle({
        width: "984px",
        height: "164px",
      });
    } finally {
      width.mockRestore();
      height.mockRestore();
    }
  });
});
