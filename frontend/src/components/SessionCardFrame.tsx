import type { ReactNode } from "react";
import { Link } from "react-router-dom";

/** Shared presentation; each source retains its own routes and authority. */
export function SessionCardFrame({
  to,
  title,
  badges,
  children,
  footer,
}: {
  to: string;
  title: string;
  badges: ReactNode;
  children: ReactNode;
  footer: ReactNode;
}) {
  return (
    <Link
      to={to}
      className="group block min-w-0 rounded-xl border border-border bg-card p-4 shadow-xs transition-all hover:-translate-y-0.5 hover:border-primary/40 hover:shadow-md focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring"
    >
      <div className="flex items-start justify-between gap-3">
        <h3 className="line-clamp-2 break-words text-base font-semibold leading-snug text-foreground group-hover:text-primary">
          {title}
        </h3>
        <div className="flex shrink-0 flex-wrap items-center gap-1.5">
          {badges}
        </div>
      </div>
      {children}
      <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        {footer}
      </div>
    </Link>
  );
}
