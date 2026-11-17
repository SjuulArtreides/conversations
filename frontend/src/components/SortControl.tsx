import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Button } from "./ui/button";
import { Chevron } from "./ui/chevron";

/**
 * Session sort order. Two fields, each ascending or descending:
 * - `activity_*` — by last activity (a session's last-message time,
 *   `ended_at`, falling back to start / upload). The default.
 * - `created_*` — by creation time (`started_at`, falling back to upload).
 */
export type SortOrder =
  "activity_desc" | "activity_asc" | "created_desc" | "created_asc";

/** The default order: most recently active first. */
export const DEFAULT_SORT: SortOrder = "activity_desc";

const LABELS: Record<SortOrder, string> = {
  activity_desc: "Last activity",
  activity_asc: "Least recent activity",
  created_desc: "Newest created",
  created_asc: "Oldest created",
};

const OPTIONS: SortOrder[] = [
  "activity_desc",
  "activity_asc",
  "created_desc",
  "created_asc",
];

interface SortControlProps {
  options?: readonly SortOrder[];
  value: SortOrder;
  onChange: (value: SortOrder) => void;
}

export function SortControl({
  value,
  onChange,
  options = OPTIONS,
}: SortControlProps) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <Button
          variant="secondary"
          className="min-w-40 justify-between"
          aria-label="Sort sessions"
        >
          <span className="truncate">{LABELS[value]}</span>
          <Chevron />
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={6}
          className="z-50 w-48 overflow-y-auto rounded-lg border border-border bg-card p-1 shadow-xl animate-fade-in"
        >
          {options.map((opt) => (
            <DropdownMenu.Item
              key={opt}
              onSelect={() => onChange(opt)}
              className="flex cursor-pointer items-center justify-between rounded-md px-2.5 py-2 text-sm outline-hidden data-highlighted:bg-accent"
            >
              <span>{LABELS[opt]}</span>
              {value === opt && <span className="text-primary">✓</span>}
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
