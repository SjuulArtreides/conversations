import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { useKeyboardInsetVar } from "./useVisualViewport";

/** Minimal stand-in for window.visualViewport with manual event dispatch. */
function makeFakeVisualViewport(initial: { height: number; offsetTop: number }) {
  const listeners: Record<string, Set<() => void>> = {
    resize: new Set(),
    scroll: new Set(),
  };
  const vv = {
    height: initial.height,
    offsetTop: initial.offsetTop,
    addEventListener: (type: string, cb: () => void) => {
      listeners[type]?.add(cb);
    },
    removeEventListener: (type: string, cb: () => void) => {
      listeners[type]?.delete(cb);
    },
    /** Test helper: change metrics and fire the matching event. */
    emit(type: "resize" | "scroll", next: Partial<{ height: number; offsetTop: number }>) {
      if (next.height != null) vv.height = next.height;
      if (next.offsetTop != null) vv.offsetTop = next.offsetTop;
      listeners[type].forEach((cb) => cb());
    },
  };
  return vv;
}

const root = () => document.documentElement;
const inset = () => root().style.getPropertyValue("--keyboard-inset");
const appHeight = () => root().style.getPropertyValue("--app-height");

beforeEach(() => {
  Object.defineProperty(window, "innerHeight", {
    value: 800,
    configurable: true,
  });
  root().style.removeProperty("--keyboard-inset");
  root().style.removeProperty("--app-height");
});

afterEach(() => {
  // @ts-expect-error — clear the stub between tests.
  delete window.visualViewport;
  root().style.removeProperty("--keyboard-inset");
  root().style.removeProperty("--app-height");
});

describe("useKeyboardInsetVar", () => {
  it("no-ops (leaves vars unset) when visualViewport is unavailable", () => {
    // jsdom has no visualViewport; don't define one.
    renderHook(() => useKeyboardInsetVar());
    expect(inset()).toBe("");
    expect(appHeight()).toBe("");
  });

  it("publishes a zero inset and full app-height when no keyboard is up", () => {
    Object.defineProperty(window, "visualViewport", {
      value: makeFakeVisualViewport({ height: 800, offsetTop: 0 }),
      configurable: true,
    });
    renderHook(() => useKeyboardInsetVar());
    // innerHeight 800, visible 800 → inset 0.
    expect(inset()).toBe("0px");
    expect(appHeight()).toBe("800px");
  });

  it("tracks the keyboard inset on a visualViewport resize", () => {
    const vv = makeFakeVisualViewport({ height: 800, offsetTop: 0 });
    Object.defineProperty(window, "visualViewport", {
      value: vv,
      configurable: true,
    });
    renderHook(() => useKeyboardInsetVar());
    expect(inset()).toBe("0px");

    // Keyboard slides up: visible area shrinks to 500 of the 800 layout height.
    act(() => vv.emit("resize", { height: 500 }));
    expect(inset()).toBe("300px"); // 800 - 500 - 0
    expect(appHeight()).toBe("500px");

    // Keyboard dismisses: back to full height, inset returns to 0.
    act(() => vv.emit("resize", { height: 800 }));
    expect(inset()).toBe("0px");
  });

  it("subtracts offsetTop so a scrolled page can't over-inset", () => {
    const vv = makeFakeVisualViewport({ height: 500, offsetTop: 100 });
    Object.defineProperty(window, "visualViewport", {
      value: vv,
      configurable: true,
    });
    renderHook(() => useKeyboardInsetVar());
    // 800 - 500 - 100 = 200.
    expect(inset()).toBe("200px");
  });

  it("clamps a negative inset to zero", () => {
    const vv = makeFakeVisualViewport({ height: 900, offsetTop: 0 });
    Object.defineProperty(window, "visualViewport", {
      value: vv,
      configurable: true,
    });
    renderHook(() => useKeyboardInsetVar());
    // 800 - 900 - 0 = -100 → clamped to 0.
    expect(inset()).toBe("0px");
  });

  it("cleans up its vars and listeners on unmount", () => {
    const vv = makeFakeVisualViewport({ height: 800, offsetTop: 0 });
    Object.defineProperty(window, "visualViewport", {
      value: vv,
      configurable: true,
    });
    const { unmount } = renderHook(() => useKeyboardInsetVar());
    expect(inset()).toBe("0px");

    unmount();
    expect(inset()).toBe("");
    expect(appHeight()).toBe("");
    // A late event after unmount must not resurrect the vars.
    act(() => vv.emit("resize", { height: 400 }));
    expect(inset()).toBe("");
  });
});
