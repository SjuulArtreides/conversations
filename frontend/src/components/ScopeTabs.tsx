import { cn } from "@/lib/utils";
import {
  SESSION_SCOPES,
  type SessionScope,
} from "@/lib/sessionScope";

interface ScopeTabsProps {
  scope: SessionScope;
  /** Per-scope counts for the badges, or null while unknown. */
  counts: Record<SessionScope, number> | null;
  onChange: (scope: SessionScope) => void;
}

/** Segmented control to switch between All / Mine / Shared with me / Organization. */
export function ScopeTabs({ scope, counts, onChange }: ScopeTabsProps) {
  return (
    <div
      role="tablist"
      aria-label="Session scope"
      className="inline-flex flex-wrap gap-1 rounded-lg border border-border bg-muted/40 p-1"
    >
      {SESSION_SCOPES.map(({ id, label }) => {
        const active = id === scope;
        const count = counts?.[id];
        return (
          <button
            key={id}
            role="tab"
            type="button"
            aria-selected={active}
            onClick={() => onChange(id)}
            className={cn(
              "rounded-md px-3 py-1.5 text-sm font-medium transition-colors focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring",
              active
                ? "bg-background text-foreground shadow-xs"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {label}
            {count != null && (
              <span
                className={cn(
                  "ml-1.5 text-xs tabular-nums",
                  active ? "text-muted-foreground" : "text-muted-foreground/70",
                )}
              >
                {count}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}
