import { cn } from "@/lib/utils";

/** Simple outline padlock. Decorative by default — the caller supplies the name. */
export function LockIcon({
  className,
  title,
  label,
}: {
  className?: string;
  /** Native SVG tooltip. */
  title?: string;
  /** Accessible name when the icon is the sole marker (e.g. the private-session glyph). */
  label?: string;
}) {
  return (
    <svg
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={cn("h-3.5 w-3.5 shrink-0", className)}
    >
      {title ? <title>{title}</title> : null}
      <rect x="4" y="11" width="16" height="9" rx="2" />
      <path d="M8 11V8a4 4 0 0 1 8 0v3" />
    </svg>
  );
}
