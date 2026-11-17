import { cn } from "@/lib/utils";

/** The one indicator for a control that opens a menu or list. Decorative: the control carries the name. */
export function Chevron({
  className,
  open,
}: {
  className?: string;
  open?: boolean;
}) {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={cn(
        "h-3.5 w-3.5 shrink-0 opacity-60 transition-transform",
        open && "rotate-180",
        className,
      )}
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  );
}
