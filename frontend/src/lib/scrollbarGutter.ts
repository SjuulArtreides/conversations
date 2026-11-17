let cachedWidth: number | null = null;

// Measured once: it is a platform property, not something that changes per
// frame. Browser zoom can change it, so a window resize drops the cache.
function scrollbarWidth(): number {
  if (cachedWidth !== null) return cachedWidth;
  const probe = document.createElement("div");
  probe.style.cssText =
    "position:absolute;visibility:hidden;width:100px;height:100px;overflow:scroll";
  document.body.appendChild(probe);
  cachedWidth = probe.offsetWidth - probe.clientWidth;
  probe.remove();
  window.addEventListener("resize", () => (cachedWidth = null), { once: true });
  return cachedWidth;
}

/**
 * Width of `el` as if its vertical scrollbar were always showing.
 *
 * The terminal's own height decides whether the page (or fullscreen box)
 * scrolls, and a scrollbar narrows the width the terminal fits into. Measuring
 * the raw width therefore lets each input flip the other. `scroller` is the
 * scrolling element, or null for the page.
 */
export function widthBesideScrollbar(
  el: HTMLElement,
  scroller: HTMLElement | null,
): number {
  const present = scroller
    ? scroller.offsetWidth - scroller.clientWidth
    : window.innerWidth - document.documentElement.clientWidth;
  const gutter = scrollbarWidth();
  // Only a gutter-sized difference is a scrollbar; anything else (zoom, a
  // layout without one) must not inflate the width.
  const showing = present > 0 && present <= gutter ? present : 0;
  return Math.max(0, el.clientWidth + showing - gutter);
}
