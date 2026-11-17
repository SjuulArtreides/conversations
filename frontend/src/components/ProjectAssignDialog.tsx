import { privateProjects } from "@/lib/privateProjects";
import { isLocalHost, localRequest } from "@/lib/localHost";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { ProjectAssign } from "./ProjectAssign";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";

export function ProjectAssignDialog({
  sessionId,
  hostId,
  onOpenChange,
}: {
  sessionId: string;
  hostId?: string;
  onOpenChange: (open: boolean) => void;
}) {
  const session = useQuery({
    queryKey: hostId
      ? ["private", hostId, "project-membership", sessionId]
      : isLocalHost()
        ? ["local-session-project", sessionId]
        : ["session", sessionId],
    queryFn: () =>
      hostId
        ? privateProjects.membership(hostId, sessionId)
        : isLocalHost()
          ? localRequest<{ project_id: string | null }>(
              `/api/sessions/${encodeURIComponent(sessionId)}/project`,
            )
          : api.getSession(sessionId),
  });
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Session project</DialogTitle>
          <DialogDescription>
            Choose a project for this conversation, or create a new one.
          </DialogDescription>
        </DialogHeader>
        {session.isPending ? (
          <p role="status">Loading project…</p>
        ) : session.isError ? (
          <p role="alert">
            Could not load the session: {session.error.message}
          </p>
        ) : (
          <ProjectAssign
            sessionId={sessionId}
            hostId={hostId}
            projectId={session.data.project_id}
            onSaved={() => onOpenChange(false)}
            onCancel={() => onOpenChange(false)}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}
