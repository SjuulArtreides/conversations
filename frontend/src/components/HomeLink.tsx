import { Link } from "react-router-dom";
import { buttonVariants } from "./ui/button";
import { cn } from "@/lib/utils";

export function HomeLink({ className }: { className?: string }) {
  return (
    <Link
      to="/"
      aria-label="Home"
      title="Home"
      className={cn(
        buttonVariants({ variant: "outline", size: "sm" }),
        "shrink-0 px-2",
        className,
      )}
    >
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="h-4 w-4"
        aria-hidden
      >
        <path d="m3 10 9-7 9 7v10a1 1 0 0 1-1 1h-5v-8H9v8H4a1 1 0 0 1-1-1z" />
      </svg>
    </Link>
  );
}
