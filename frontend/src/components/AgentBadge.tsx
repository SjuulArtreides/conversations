import type { AgentKind } from "@/lib/types";
import { cn } from "@/lib/utils";
import { Badge } from "./ui/badge";

/**
 * Marks which agent produced a session. Claude is the implied default for every
 * legacy session, so badging it everywhere would just be noise. Non-Claude
 * sources get a chip so mixed-agent history stays clear at a glance.
 */
export function AgentBadge({
  agent,
  className,
}: {
  agent: AgentKind | undefined;
  className?: string;
}) {
  if (agent === "codex") {
    return (
      <Badge
        variant="outline"
        title="Recorded from a Codex session"
        className={cn(
          "whitespace-nowrap shrink-0 border-transparent bg-violet-500/15 text-violet-500 dark:text-violet-300",
          className,
        )}
      >
        ✦ Codex
      </Badge>
    );
  }
  if (agent === "opencode") {
    return (
      <Badge
        variant="outline"
        title="Recorded from an OpenCode session"
        className={cn(
          "whitespace-nowrap shrink-0 border-transparent bg-sky-500/15 text-sky-600 dark:text-sky-300",
          className,
        )}
      >
        ◇ OpenCode
      </Badge>
    );
  }
  if (agent === "zsh") {
    return (
      <Badge
        variant="outline"
        title="Recorded from a shell session"
        className={cn(
          "whitespace-nowrap shrink-0 border-transparent bg-green-500/15 text-green-600 dark:text-green-400",
          className,
        )}
      >
        $ Zsh
      </Badge>
    );
  }
  return null;
}
