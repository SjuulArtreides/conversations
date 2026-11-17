import type { ReactNode } from "react";
import { SessionActions } from "./SessionActions";

/** Shared detail presentation; source adapters supply real actions and metadata. */
export function SessionDetailHeader({
  title,
  badges,
  actions,
  project,
  children,
}: {
  title: string;
  badges?: ReactNode;
  actions?: ReactNode;
  project?: string | null;
  children?: ReactNode;
}) {
  return (
    <div className="rounded-xl border border-border bg-card p-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <h1 className="break-words text-xl font-semibold leading-snug tracking-tight">
            {title}
          </h1>
          {badges}
        </div>
        <SessionActions>{actions}</SessionActions>
      </div>
      {project && (
        <div
          className="mt-2 font-mono text-xs text-muted-foreground"
          title={project}
        >
          {project}
        </div>
      )}
      {children}
    </div>
  );
}
