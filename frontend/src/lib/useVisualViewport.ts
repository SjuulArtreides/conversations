import * as React from "react";

/**
 * Keep CSS custom properties on `<html>` in sync with the visual viewport so
 * fixed, full-height panels can sit ABOVE the on-screen keyboard instead of
 * behind it.
 *
 * The problem this solves: on a phone the soft keyboard shrinks the *visual*
 * viewport but NOT the layout viewport — `window.innerHeight`, `100vh` and even
 * `100dvh` all stay full-size — and the browser does not scroll
 * `position: fixed` elements to keep a focused field visible. A bottom-anchored
 * input inside a fixed panel therefore ends up hidden behind the keyboard: you
 * type but can't see the text. `window.visualViewport` is the only reliable
 * signal for the keyboard's size, so we publish it as two CSS variables on the
 * document root:
 *
 *   --keyboard-inset : pixels between the layout-viewport bottom and the
 *                      keyboard top (0 when no keyboard is up). Use it as a
 *                      fixed panel's `bottom` to lift its content clear of the
 *                      keyboard.
 *   --app-height     : the visible viewport height in px (the on-screen area
 *                      above the keyboard). Use it where `100vh` would
 *                      over-shoot on mobile.
 *
 * No-op — leaving both variables unset so CSS fallbacks (`100%` / `0px`) apply —
 * when `window.visualViewport` is unavailable (older browsers, SSR, jsdom), so
 * the default behaviour is preserved everywhere it isn't needed. Desktop
 * browsers expose `visualViewport` but it never shrinks (no soft keyboard), so
 * the inset stays `0` and nothing changes there either.
 *
 * Call once near the app root.
 */
export function useKeyboardInsetVar(): void {
  React.useEffect(() => {
    const vv =
      typeof window !== "undefined" ? window.visualViewport : undefined;
    if (!vv) return;

    const root = document.documentElement;
    const update = () => {
      // Keyboard height = layout viewport - (visible area + its top offset).
      // Clamp to 0 so a transient negative (e.g. rubber-band scroll) can't push
      // the panel down past the bottom edge.
      const inset = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
      root.style.setProperty("--keyboard-inset", `${Math.round(inset)}px`);
      root.style.setProperty("--app-height", `${Math.round(vv.height)}px`);
    };

    update();
    // `resize` fires as the keyboard animates in/out; `scroll` keeps the offset
    // honest when the page is scrolled under the (fixed) keyboard.
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    return () => {
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
      root.style.removeProperty("--keyboard-inset");
      root.style.removeProperty("--app-height");
    };
  }, []);
}
