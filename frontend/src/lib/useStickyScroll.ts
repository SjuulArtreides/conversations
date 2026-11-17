import { useCallback, useEffect, useRef, useState } from "react";

/** Distance from each extreme (px) within which we consider the page "at top" or "at bottom". */
const THRESHOLD = 32;

export interface UseStickyScrollResult {
  atTop: boolean;
  atBottom: boolean;
  scrollToTop: () => void;
  scrollToBottom: () => void;
  /**
   * Scroll to the bottom ONLY when the user is currently pinned at (or very
   * close to) the bottom. Call this whenever new content arrives so the view
   * tracks incoming messages without yanking a user who scrolled up.
   */
  stickToBottomIfPinned: () => void;
}

function getScrollMetrics() {
  if (typeof window === "undefined") {
    return { scrollY: 0, innerHeight: 0, scrollHeight: 0 };
  }
  return {
    scrollY: window.scrollY,
    innerHeight: window.innerHeight,
    scrollHeight: document.documentElement.scrollHeight,
  };
}

/**
 * Hook that tracks window-level scroll position and exposes helpers for
 * auto-scrolling a full-page transcript view.
 *
 * Operates on the WINDOW scroll (the live-session view has no inner scroll
 * container). Listeners are cleaned up on unmount. Safe to use in SSR
 * environments — `window` is guarded before access.
 */
export function useStickyScroll(): UseStickyScrollResult {
  const [atTop, setAtTop] = useState(true);
  const [atBottom, setAtBottom] = useState(true);

  // A ref so stickToBottomIfPinned always reads the latest value without
  // needing to be recreated whenever atBottom changes.
  const atBottomRef = useRef(true);

  const update = useCallback(() => {
    const { scrollY, innerHeight, scrollHeight } = getScrollMetrics();
    const distFromTop = scrollY;
    const distFromBottom = scrollHeight - innerHeight - scrollY;

    const nowAtTop = distFromTop <= THRESHOLD;
    const nowAtBottom = distFromBottom <= THRESHOLD;

    setAtTop(nowAtTop);
    setAtBottom(nowAtBottom);
    atBottomRef.current = nowAtBottom;
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;

    // Compute initial state synchronously.
    update();

    let rafHandle: number | null = null;

    const onScroll = () => {
      if (rafHandle !== null) return;
      rafHandle = requestAnimationFrame(() => {
        rafHandle = null;
        update();
      });
    };

    window.addEventListener("scroll", onScroll, { passive: true });
    // Also listen for resize: the page becoming taller/shorter changes
    // distFromBottom without a scroll event firing.
    window.addEventListener("resize", onScroll, { passive: true });

    return () => {
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
      if (rafHandle !== null) cancelAnimationFrame(rafHandle);
    };
  }, [update]);

  const scrollToTop = useCallback(() => {
    if (typeof window === "undefined") return;
    window.scrollTo({ top: 0, behavior: "smooth" });
  }, []);

  const scrollToBottom = useCallback(() => {
    if (typeof window === "undefined") return;
    window.scrollTo({
      top: document.documentElement.scrollHeight,
      behavior: "smooth",
    });
  }, []);

  const stickToBottomIfPinned = useCallback(() => {
    if (atBottomRef.current) {
      if (typeof window === "undefined") return;
      window.scrollTo({ top: document.documentElement.scrollHeight });
    }
  }, []);

  return { atTop, atBottom, scrollToTop, scrollToBottom, stickToBottomIfPinned };
}
