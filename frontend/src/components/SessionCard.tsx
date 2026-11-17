import { SessionCardFrame } from "./SessionCardFrame";
import type { Summary } from "@/lib/types";
import { fullTimestamp, humanizeTime, shortProject } from "@/lib/utils";
import { Badge } from "./ui/badge";
import { AgentBadge } from "./AgentBadge";
import { VisibilityBadge } from "./VisibilityBadge";
import { LiveBadge } from "./LiveBadge";

export function SessionCard({
  session,
  live = false,
}: {
  session: Summary;
  /** Whether this session is currently streaming live (drives the badge). */
  live?: boolean;
}) {
  const workRepos = session.repos.filter((r) => r.is_container !== true);
  // Fall back to the transcript branch only when there are no work repos AND the
  // session wasn't launched at a container root — a container's branch is not a
  // work repo's, so showing it reads as if the container were the work repo.
  const showTranscriptBranch =
    session.git_branch &&
    workRepos.length === 0 &&
    session.project_is_container !== true;

  // A live card opens the live terminal directly: clicking it is the same
  // intent as entering fullscreen, and `?fullscreen=1` lets the detail route
  // paint the terminal shell on first render instead of flashing history under
  // a spinner while the live-sessions poll and PTY transport resolve.
  const to = live
    ? `/sessions/${encodeURIComponent(session.id)}?fullscreen=1`
    : `/sessions/${encodeURIComponent(session.id)}`;

  return (
    <SessionCardFrame
      to={to}
      title={session.title}
      badges={
        <>
          {live && <LiveBadge size="sm" />}
          <VisibilityBadge visibility={session.visibility} />
        </>
      }
      footer={
        <>
          <span title={fullTimestamp(session.started_at)}>
            {humanizeTime(session.started_at)}
          </span>
          <span>·</span>
          <span>uploaded by {session.uploaded_by || "anonymous"}</span>
        </>
      }
    >
      <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
        <AgentBadge agent={session.agent} />
        {session.source_host && (
          <Badge
            variant="outline"
            className="max-w-32 truncate"
            title={session.source_host}
            aria-label={`Host: ${session.source_host}`}
          >
            {session.source_host}
          </Badge>
        )}
        {session.project_path && (
          <span
            className="font-mono text-foreground/80"
            title={session.project_path}
          >
            {shortProject(session.project_path)}
          </span>
        )}
        {showTranscriptBranch && (
          <Badge variant="branch" className="py-0">
            ⎇ {session.git_branch}
          </Badge>
        )}
        {session.message_count > 0 && (
          <>
            <span>·</span>
            <span>{session.message_count.toLocaleString()} messages</span>
          </>
        )}
      </div>

      {workRepos.length > 0 && (
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          {workRepos.slice(0, 4).map((r) => (
            <Badge
              key={`${r.repo}@${r.branch}`}
              variant="branch"
              className="py-0"
              title={r.remote_url ?? undefined}
            >
              {r.repo} ⎇ {r.branch}
            </Badge>
          ))}
          {workRepos.length > 4 && (
            <span className="text-xs text-muted-foreground">
              +{workRepos.length - 4} more
            </span>
          )}
        </div>
      )}
    </SessionCardFrame>
  );
}
