import { cn } from "@/lib/utils";
import type { AgentReadiness } from "@/lib/types";

const labels = {
  unknown: "Status unknown",
  working: "Working",
  background: "Background work",
  needs_input: "Needs you",
  ready: "Ready",
  disconnected: "Disconnected",
};
const colors = {
  unknown: "bg-muted-foreground/50",
  working: "bg-red-500",
  background: "bg-amber-500",
  needs_input: "bg-blue-500",
  ready: "bg-emerald-500",
  disconnected: "bg-muted-foreground/40",
};

export function AgentReadinessIndicator({
  readiness,
  connected = true,
  showLabel = false,
}: {
  readiness?: AgentReadiness;
  connected?: boolean;
  showLabel?: boolean;
}) {
  const state = connected ? (readiness ?? "unknown") : "disconnected";
  const label = labels[state];
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1.5 text-xs"
      title={label}
    >
      <span
        role="img"
        aria-label={label}
        className={cn(
          "inline-block h-1.5 w-1.5 shrink-0 rounded-full",
          colors[state],
          (state === "working" || state === "background") &&
            "motion-safe:animate-pulse",
        )}
      />
      {showLabel && <span>{label}</span>}
    </span>
  );
}
