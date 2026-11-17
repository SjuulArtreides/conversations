import { cn } from "@/lib/utils";

/**
 * A "● LIVE" indicator with a pulsing dot, shown on a session that is currently
 * streaming. `size="sm"` is the card variant; the default is the header variant.
 *
 * When `interrupted` is set, the session is still live (per the DB status) but
 * no CLI is attached to inject replies into — render an amber "⚠ Interrupted"
 * badge instead of the red pulsing "● LIVE".
 */
export function LiveBadge({
  size = "md",
  interrupted = false,
  className,
}: {
  size?: "sm" | "md";
  interrupted?: boolean;
  className?: string;
}) {
  const sizeClasses =
    size === "sm" ? "px-2 py-0.5 text-[0.65rem]" : "px-2.5 py-0.5 text-xs";

  if (interrupted) {
    return (
      <span
        title="No CLI is currently attached to this session."
        className={cn(
          "inline-flex items-center gap-1.5 rounded-full border border-amber-500/40 bg-amber-500/15 font-semibold uppercase tracking-wide text-amber-500 dark:text-amber-300",
          sizeClasses,
          className,
        )}
      >
        <span aria-hidden>⚠</span>
        Interrupted
      </span>
    );
  }

  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border border-red-500/40 bg-red-500/15 font-semibold uppercase tracking-wide text-red-400 dark:text-red-300",
        sizeClasses,
        className,
      )}
    >
      <span className="relative flex h-2 w-2" aria-hidden>
        <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
        <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
      </span>
      Live
    </span>
  );
}
