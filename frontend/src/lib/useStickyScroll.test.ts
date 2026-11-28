import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useStickyScroll } from "./useStickyScroll";

// jsdom window scroll helpers
function setScrollMetrics({
  scrollY,
  innerHeight,
  scrollHeight,
}: {
  scrollY: number;
  innerHeight: number;
  scrollHeight: number;
}) {
  Object.defineProperty(window, "scrollY", { value: scrollY, configurable: true });
  Object.defineProperty(window, "innerHeight", {
    value: innerHeight,
    configurable: true,
  });
  Object.defineProperty(document.documentElement, "scrollHeight", {
    value: scrollHeight,
    configurable: true,
  });
}

function fireScroll() {
  window.dispatchEvent(new Event("scroll"));
}

beforeEach(() => {
  // Start in a state where the page is effectively at the top.
  setScrollMetrics({ scrollY: 0, innerHeight: 600, scrollHeight: 600 });
  // Stub scrollTo so tests can assert on it without jsdom errors.
  vi.stubGlobal("scrollTo", vi.fn());
  // Make rAF run synchronously so scroll-handler state updates are observable
  // within a single act() call without extra timer gymnastics.
  vi.stubGlobal(
    "requestAnimationFrame",
    (cb: FrameRequestCallback) => {
      cb(0);
      return 0;
    },
  );
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useStickyScroll — initial state", () => {
  it("starts atTop=true, atBottom=true when page is too short to scroll", async () => {
    // scrollHeight === innerHeight → 0px of scroll room → both extremes true.
    setScrollMetrics({ scrollY: 0, innerHeight: 600, scrollHeight: 600 });
    const { result } = renderHook(() => useStickyScroll());
    // Allow the synchronous update() call inside useEffect to flush.
    await act(async () => {});
    expect(result.current.atTop).toBe(true);
    expect(result.current.atBottom).toBe(true);
  });
});

describe("useStickyScroll — atTop", () => {
  it("atTop=true when scrollY is within threshold (16px)", async () => {
    setScrollMetrics({ scrollY: 16, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atTop).toBe(true);
  });

  it("atTop=false when scrollY exceeds threshold (33px)", async () => {
    setScrollMetrics({ scrollY: 33, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atTop).toBe(false);
  });
});

describe("useStickyScroll — atBottom", () => {
  it("atBottom=true when distFromBottom is within threshold (20px)", async () => {
    // scrollY=580, innerHeight=600, scrollHeight=1200 → dist = 1200-600-580 = 20
    setScrollMetrics({ scrollY: 580, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atBottom).toBe(true);
  });

  it("atBottom=false when distFromBottom exceeds threshold (100px)", async () => {
    // scrollY=500, innerHeight=600, scrollHeight=1200 → dist = 1200-600-500 = 100
    setScrollMetrics({ scrollY: 500, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atBottom).toBe(false);
  });
});

describe("useStickyScroll — scroll position changes update state", () => {
  it("flips atTop from true to false when the user scrolls down past threshold", async () => {
    setScrollMetrics({ scrollY: 0, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {});
    expect(result.current.atTop).toBe(true);

    // Scroll past the threshold; then flush the rAF callback by dispatching
    // another scroll event (rAF runs synchronously in jsdom after the first
    // callback resolves, so a second act flushes any pending callbacks).
    await act(async () => {
      setScrollMetrics({ scrollY: 200, innerHeight: 600, scrollHeight: 1200 });
      fireScroll();
    });
    expect(result.current.atTop).toBe(false);
  });

  it("flips atBottom from false to true when user scrolls to the bottom", async () => {
    setScrollMetrics({ scrollY: 0, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {});
    // Not at bottom initially.
    expect(result.current.atBottom).toBe(false);

    await act(async () => {
      // Scroll to bottom: dist = 1200-600-600 = 0
      setScrollMetrics({ scrollY: 600, innerHeight: 600, scrollHeight: 1200 });
      fireScroll();
    });
    expect(result.current.atBottom).toBe(true);
  });
});

describe("useStickyScroll — scrollToTop / scrollToBottom helpers", () => {
  it("scrollToTop calls window.scrollTo({ top: 0 })", async () => {
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {});
    act(() => result.current.scrollToTop());
    expect(window.scrollTo).toHaveBeenCalledWith(
      expect.objectContaining({ top: 0 }),
    );
  });

  it("scrollToBottom calls window.scrollTo({ top: scrollHeight })", async () => {
    setScrollMetrics({ scrollY: 0, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {});
    act(() => result.current.scrollToBottom());
    expect(window.scrollTo).toHaveBeenCalledWith(
      expect.objectContaining({ top: 1200 }),
    );
  });
});

describe("useStickyScroll — stickToBottomIfPinned", () => {
  it("scrolls to bottom when currently pinned (atBottom=true)", async () => {
    // Start at the bottom: dist = 0
    setScrollMetrics({ scrollY: 600, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atBottom).toBe(true);

    vi.mocked(window.scrollTo).mockClear();
    act(() => result.current.stickToBottomIfPinned());
    expect(window.scrollTo).toHaveBeenCalledWith({ top: 1200 });
  });

  it("does NOT scroll when the user has scrolled up (atBottom=false)", async () => {
    // Start somewhere in the middle: dist = 1200-600-200 = 400
    setScrollMetrics({ scrollY: 200, innerHeight: 600, scrollHeight: 1200 });
    const { result } = renderHook(() => useStickyScroll());
    await act(async () => {
      fireScroll();
    });
    expect(result.current.atBottom).toBe(false);

    vi.mocked(window.scrollTo).mockClear();
    act(() => result.current.stickToBottomIfPinned());
    expect(window.scrollTo).not.toHaveBeenCalled();
  });
});
