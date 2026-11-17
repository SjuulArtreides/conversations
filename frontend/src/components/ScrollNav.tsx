/**
 * Floating scroll-navigation control.
 *
 * Renders up to two small icon buttons in the bottom-right corner of the
 * viewport. Each button is only rendered when it makes sense:
 *   ↑ (scroll to top)    — hidden when already at / near the top
 *   ↓ (scroll to bottom) — hidden when already at / near the bottom
 *
 * If neither button is visible the component renders nothing.
 */
export interface ScrollNavProps {
  showUp: boolean;
  showDown: boolean;
  onUp: () => void;
  onDown: () => void;
}

export function ScrollNav({ showUp, showDown, onUp, onDown }: ScrollNavProps) {
  if (!showUp && !showDown) return null;

  return (
    <div className="fixed bottom-24 right-6 z-30 flex flex-col gap-2">
      {showUp && (
        <button
          type="button"
          aria-label="Scroll to top"
          onClick={onUp}
          className="flex h-9 w-9 items-center justify-center rounded-full border border-border bg-card text-muted-foreground shadow-md transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring"
        >
          {/* Chevron up */}
          <svg
            className="h-4 w-4"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            aria-hidden
          >
            <path d="m18 15-6-6-6 6" />
          </svg>
        </button>
      )}
      {showDown && (
        <button
          type="button"
          aria-label="Scroll to bottom"
          onClick={onDown}
          className="flex h-9 w-9 items-center justify-center rounded-full border border-border bg-card text-muted-foreground shadow-md transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring"
        >
          {/* Chevron down */}
          <svg
            className="h-4 w-4"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            aria-hidden
          >
            <path d="m6 9 6 6 6-6" />
          </svg>
        </button>
      )}
    </div>
  );
}
