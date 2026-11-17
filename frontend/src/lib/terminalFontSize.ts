import * as React from "react";

/**
 * The viewer's preferred live-terminal font size, kept per browser: the right
 * size depends on the screen, so a laptop and a phone keep their own value.
 *
 * The terminal renders at this size when the source grid fits, and "Fit
 * columns and rows to screen" sizes the grid so it fills the screen at it.
 * Unset, the terminal keeps its built-in defaults.
 */
export const TERMINAL_FONT_SIZE_KEY = "arrakisTerminalFontSize";
export const MIN_PREFERRED_FONT_PX = 8;
export const MAX_PREFERRED_FONT_PX = 32;

/** Same-tab change notification; `storage` events only reach other tabs. */
const CHANGE_EVENT = "arrakis-terminal-font-size";

function parse(raw: string | null): number | null {
  if (raw === null || raw.trim() === "") return null;
  const value = Number(raw);
  if (!Number.isInteger(value)) return null;
  if (value < MIN_PREFERRED_FONT_PX || value > MAX_PREFERRED_FONT_PX)
    return null;
  return value;
}

/** The saved preference, or `null` when unset or unreadable. */
export function readPreferredFontSize(): number | null {
  try {
    return parse(localStorage.getItem(TERMINAL_FONT_SIZE_KEY));
  } catch {
    return null;
  }
}

/** Save `px` (clamped to the allowed range), or clear it with `null`. */
export function writePreferredFontSize(px: number | null): void {
  try {
    if (px === null) {
      localStorage.removeItem(TERMINAL_FONT_SIZE_KEY);
    } else {
      const clamped = Math.min(
        MAX_PREFERRED_FONT_PX,
        Math.max(MIN_PREFERRED_FONT_PX, Math.round(px)),
      );
      localStorage.setItem(TERMINAL_FONT_SIZE_KEY, String(clamped));
    }
  } catch {
    // ignore — quota or private-browsing restriction
  }
  window.dispatchEvent(new Event(CHANGE_EVENT));
}

function subscribe(onChange: () => void): () => void {
  const onStorage = (event: StorageEvent) => {
    if (event.key === null || event.key === TERMINAL_FONT_SIZE_KEY) onChange();
  };
  window.addEventListener(CHANGE_EVENT, onChange);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(CHANGE_EVENT, onChange);
    window.removeEventListener("storage", onStorage);
  };
}

/** The saved preference, live across this tab and others. */
export function usePreferredFontSize(): number | null {
  return React.useSyncExternalStore(
    subscribe,
    readPreferredFontSize,
    () => null,
  );
}
