import * as React from "react";
import { cn } from "@/lib/utils";

interface CollapsibleProps {
  label: React.ReactNode;
  defaultOpen?: boolean;
  className?: string;
  summaryClassName?: string;
  children: React.ReactNode;
}

/** Accessible disclosure built on the native <details> element. */
export function Collapsible({
  label,
  defaultOpen = false,
  className,
  summaryClassName,
  children,
}: CollapsibleProps) {
  return (
    <details
      open={defaultOpen}
      className={cn(
        "group rounded-lg border border-border bg-muted/30",
        className,
      )}
    >
      <summary
        className={cn(
          "flex cursor-pointer select-none items-center gap-2 px-3 py-2 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground [&::-webkit-details-marker]:hidden",
          summaryClassName,
        )}
      >
        <svg
          className="h-3 w-3 shrink-0 transition-transform group-open:rotate-90"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <path d="m9 18 6-6-6-6" />
        </svg>
        {label}
      </summary>
      <div className="border-t border-border px-3 py-2">{children}</div>
    </details>
  );
}
