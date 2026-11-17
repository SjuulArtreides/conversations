import type { Visibility } from "@/lib/types";
import { Badge } from "./ui/badge";

export function VisibilityBadge({ visibility }: { visibility: Visibility }) {
  if (visibility === "organization") {
    return (
      <Badge
        variant="org"
        title="Visible to the whole organization"
        className="whitespace-nowrap shrink-0"
      >
        🌐 Organization
      </Badge>
    );
  }
  return (
    <Badge
      variant="restricted"
      title="Shared with specific people"
      className="whitespace-nowrap shrink-0"
    >
      🔒 Restricted
    </Badge>
  );
}
