import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { DetectedPr, PrDetectProvider, type PrDetectValue } from "./PrDetect";
import { prAnchorId } from "@/lib/prUrls";

const ref = {
  url: "https://github.com/o/r/pull/42",
  owner: "o",
  repo: "r",
  number: 42,
};

const scrollSpy = vi.fn();

beforeEach(() => {
  // jsdom has no scrollIntoView — stub it so the jump helper can run.
  Element.prototype.scrollIntoView = scrollSpy;
  scrollSpy.mockClear();
});

afterEach(() => vi.useRealTimers());

function renderLinked(children: React.ReactNode) {
  const value: PrDetectValue = {
    enabled: true,
    repoCtx: null,
    linkedUrls: new Set([ref.url]),
    canAdd: false,
    onAdd: async () => {},
  };
  return render(
    <PrDetectProvider value={value}>
      <DetectedPr prRef={ref}>{children}</DetectedPr>
      {/* The row the jump scrolls to. */}
      <div id={prAnchorId(ref.owner, ref.repo, ref.number)} data-testid="row" />
    </PrDetectProvider>,
  );
}

describe("DetectedPr — jump to linked PR", () => {
  it("scrolls to the row and flashes it when the ✓ is clicked", () => {
    renderLinked("PR #42");
    fireEvent.click(screen.getByRole("button", { name: /jump to pr #42/i }));
    expect(scrollSpy).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("row").className).toMatch(/ring-2/);
  });

  it("scrolls when the chip text (short ref, no link) is clicked", () => {
    renderLinked("PR #42");
    fireEvent.click(screen.getByText("PR #42"));
    expect(scrollSpy).toHaveBeenCalledTimes(1);
  });

  it("does NOT scroll when the inner blue link is clicked (keeps the link)", () => {
    renderLinked(<a href={ref.url}>{ref.url}</a>);
    fireEvent.click(screen.getByRole("link", { name: ref.url }));
    expect(scrollSpy).not.toHaveBeenCalled();
  });

  it("still scrolls via the ✓ even in the URL style", () => {
    renderLinked(<a href={ref.url}>{ref.url}</a>);
    fireEvent.click(screen.getByRole("button", { name: /jump to pr #42/i }));
    expect(scrollSpy).toHaveBeenCalledTimes(1);
  });
});
