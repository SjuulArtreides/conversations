import type { PrStatus } from "@/lib/types";

export function PrReviewIcon({ status }: { status: PrStatus | null }) {
  const decision = status?.review_decision;
  if (decision !== "APPROVED" && decision !== "CHANGES_REQUESTED") return null;
  const approved = decision === "APPROVED";
  const label = approved ? "Approved" : "Changes requested";
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className={`ml-auto shrink-0 ${approved ? "text-green-400" : "text-red-400"}`}
    >
      <svg
        aria-hidden="true"
        width="16"
        height="16"
        viewBox="0 0 16 16"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        {approved ? (
          <path d="m3 8 3 3 7-7" />
        ) : (
          <>
            <circle cx="8" cy="8" r="6" />
            <path d="m6 6 4 4m0-4-4 4" />
          </>
        )}
      </svg>
    </span>
  );
}
