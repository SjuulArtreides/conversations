import type { ReactNode } from "react";

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 py-20 text-muted-foreground">
      <div className="h-7 w-7 animate-spin rounded-full border-2 border-border border-t-primary" />
      {label && <p className="text-sm">{label}</p>}
    </div>
  );
}

export function ErrorState({
  title = "Something went wrong",
  message,
  onRetry,
  action,
}: {
  title?: string;
  message?: string;
  onRetry?: () => void;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 rounded-xl border border-destructive/30 bg-destructive/10 py-16 text-center">
      <p className="text-base font-semibold">{title}</p>
      {message && (
        <p className="max-w-md text-sm text-muted-foreground">{message}</p>
      )}
      {action}
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          className="mt-1 rounded-md border border-border px-3 py-1.5 text-sm transition-colors hover:bg-accent"
        >
          Retry
        </button>
      )}
    </div>
  );
}

export function EmptyState({
  title,
  message,
}: {
  title: string;
  message?: string;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 rounded-xl border border-dashed border-border py-20 text-center">
      <div className="text-3xl opacity-60">🗂️</div>
      <p className="text-base font-medium">{title}</p>
      {message && (
        <p className="max-w-md text-sm text-muted-foreground">{message}</p>
      )}
    </div>
  );
}
