import { AgentReadinessIndicator } from "./AgentReadinessIndicator";
import { SessionCardFrame } from "./SessionCardFrame";
import type { SessionEntry } from "@/lib/privateChannel";
import { ALL_AGENTS } from "@/lib/types";
import { fullTimestamp, humanizeTime, shortProject } from "@/lib/utils";
import { AgentBadge } from "./AgentBadge";
import { LiveBadge } from "./LiveBadge";
import { Badge } from "./ui/badge";

const agentNames: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  opencode: "OpenCode",
  zsh: "Shell",
};

/** Defend against older hosts using a bootstrap message or native ID as a title. */
export function privateSessionTitle(session: SessionEntry): string {
  const title = session.title?.replace(/\s+/gu, " ").trim() ?? "";
  const bootstrap =
    /^(?:#+\s*)?(?:AGENTS\.md\b|CLAUDE\.md\b|<\/?(?:instructions|environment_context|INSTRUCTIONS|system|developer|user_instructions|skills_instructions)\b|You are (?:Codex|Claude|an? (?:AI|coding|agent))\b)/i;
  const uuid =
    /^(?:[a-z]+[-_:])?[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  if (
    !title ||
    title === session.id ||
    uuid.test(title) ||
    bootstrap.test(title)
  ) {
    return `${agentNames[session.agent] ?? "Session"} · ${shortProject(session.cwd)}`;
  }
  const chars = Array.from(title);
  return chars.length > 120 ? `${chars.slice(0, 119).join("")}…` : title;
}

export function PrivateSessionCard({
  session,
  hostId,
  hostLabel,
}: {
  session: SessionEntry;
  hostId?: string;
  hostLabel?: string;
}) {
  const agent = ALL_AGENTS.find((a) => a === session.agent);
  const activity =
    session.last_activity && Number.isFinite(Date.parse(session.last_activity))
      ? session.last_activity
      : null;
  // A running card opens the live terminal directly: clicking it is the same
  // intent as entering fullscreen, and `?fullscreen=1` lets the detail route
  // paint the terminal shell on first render instead of flashing history under
  // a spinner while the host responds and PTY transport resolves.
  const params = new URLSearchParams();
  if (session.running) params.set("fullscreen", "1");
  if (hostId) params.set("host", hostId);
  const to = `/private/${encodeURIComponent(session.id)}${params.size ? `?${params}` : ""}`;

  return (
    <SessionCardFrame
      to={to}
      title={privateSessionTitle(session)}
      badges={
        <>
          {session.running && <LiveBadge size="sm" />}
          {session.running && (
            <AgentReadinessIndicator readiness={session.readiness} showLabel />
          )}
          <Badge variant="outline">Private</Badge>
        </>
      }
      footer={
        <>
          {activity ? (
            <span title={fullTimestamp(activity)}>
              Active {humanizeTime(activity)}
            </span>
          ) : session.running ? (
            "Running on your host"
          ) : (
            "Stopped"
          )}
        </>
      }
    >
      <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
        <AgentBadge agent={agent} />
        {hostLabel && (
          <Badge
            variant="outline"
            className="max-w-32 truncate"
            title={hostLabel}
            aria-label={`Host: ${hostLabel}`}
          >
            {hostLabel}
          </Badge>
        )}
        {agent === "claude" && <Badge variant="outline">Claude</Badge>}
        <span
          className="max-w-full truncate font-mono text-foreground/80"
          title={session.cwd}
        >
          {shortProject(session.cwd)}
        </span>
        <span>·</span>
        <span>{session.messages.toLocaleString()} messages</span>
      </div>
    </SessionCardFrame>
  );
}
