import { useSearchParams } from "react-router-dom";
import { Button } from "@/components/ui/button";

/**
 * A fullscreen overlay shown while the live terminal is not yet mountable
 * (host connection / session metadata still loading, or the session has
 * ended). Painted on first render for `?fullscreen=1` URLs so the normal
 * detail header, metadata cards, history, and reply composer never flash
 * before the terminal takes over.
 *
 * Matches `LiveTerminal`'s fullscreen look (same fixed overlay + dark
 * background) so the handover between this shell and the real terminal
 * is a replacement, not a pop-in.
 */
export function FullscreenPending({
  title,
  status,
  action,
}: {
  action?: React.ReactNode;
  /** Optional title shown next to the exit control (e.g. the session title). */
  title?: string;
  /**
   * What to show in the centre. Default is a spinner + "Connecting…"; pass
   * `kind: "stopped"` or `kind: "error"` to keep the user in fullscreen
   * context when the session cannot start (instead of dropping to normal page).
   */
  status?:
    | { kind: "connecting"; label?: string }
    | { kind: "stopped"; label?: string }
    | { kind: "error"; label?: string };
}) {
  const state = status ?? { kind: "connecting" as const };
  const [, setSearchParams] = useSearchParams();
  const exitFullscreen = () => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.delete("fullscreen");
      return next;
    });
  };
  const label =
    state.label ??
    (state.kind === "connecting"
      ? "Connecting to terminal…"
      : state.kind === "stopped"
        ? "This session has ended."
        : "Terminal is unavailable.");
  return (
    <div
      data-testid="fullscreen-pending"
      className="fixed inset-0 z-60 flex flex-col bg-[#0d1117] p-4"
    >
      <div className="mb-3 flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1 truncate text-sm text-muted-foreground">
          {title ?? ""}
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={exitFullscreen}
        >
          Exit fullscreen
        </Button>
      </div>
      <div
        role="status"
        aria-live="polite"
        className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground"
      >
        {state.kind === "connecting" && (
          <div
            aria-hidden="true"
            className="h-7 w-7 animate-spin rounded-full border-2 border-border border-t-primary"
          />
        )}
        <p className="text-sm">{label}</p>
        {action}
      </div>
    </div>
  );
}
