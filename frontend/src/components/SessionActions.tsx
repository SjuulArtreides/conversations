import * as React from "react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Chevron } from "@/components/ui/chevron";

interface SessionActionsProps {
  children: React.ReactNode;
  className?: string;
}

/**
 * Keeps session actions inline on larger screens and behind an explicit,
 * accessible collapsible on narrow screens.
 */
export function SessionActions({ children, className }: SessionActionsProps) {
  const [mobileOpen, setMobileOpen] = React.useState(false);
  const actionsId = React.useId();

  // Conditional children render as null/false; with none left there is
  // nothing to expand, so the toggle would open an empty box.
  if (React.Children.toArray(children).length === 0) return null;

  return (
    <div className="w-full md:w-auto">
      <Button
        type="button"
        size="sm"
        variant="outline"
        aria-controls={actionsId}
        aria-expanded={mobileOpen}
        onClick={() => setMobileOpen((open) => !open)}
        className="w-full justify-between md:hidden"
      >
        Session actions
        <Chevron open={mobileOpen} />
      </Button>
      <div
        id={actionsId}
        className={cn(
          "flex-wrap items-center gap-2 md:flex",
          mobileOpen ? "mt-2 flex" : "hidden",
          "md:mt-0",
          className,
        )}
      >
        {children}
      </div>
    </div>
  );
}
