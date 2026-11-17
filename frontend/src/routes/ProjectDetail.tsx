import * as React from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useAuth, useRedirectOnUnauthorized } from "@/lib/useAuth";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { VisibilityBadge } from "@/components/VisibilityBadge";
import { ChatPanel } from "@/components/ChatPanel";
import { ErrorState, Spinner } from "@/components/states";
import { useToast } from "@/components/ui/toast";
import { humanizeTime, shortProject } from "@/lib/utils";

export function ProjectDetail() {
  const { id = "" } = useParams();
  const { user } = useAuth();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const { data, isLoading, isError, error, refetch } = useQuery({
    queryKey: ["project", id],
    queryFn: () => api.getProject(id),
    enabled: !!id,
  });

  useRedirectOnUnauthorized(error);

  const [editing, setEditing] = React.useState(false);
  const [name, setName] = React.useState("");
  const [description, setDescription] = React.useState("");

  React.useEffect(() => {
    if (data) {
      setName(data.project.name);
      setDescription(data.project.description ?? "");
    }
  }, [data]);

  const rename = useMutation({
    mutationFn: (patch: { name: string; description?: string }) =>
      api.updateProject(id, patch),
    onSuccess: () => {
      setEditing(false);
      queryClient.invalidateQueries({ queryKey: ["project", id] });
      queryClient.invalidateQueries({ queryKey: ["projects-explicit"] });
    },
    onError: (err: Error) => toast(`Could not update project: ${err.message}`, "error"),
  });

  const remove = useMutation({
    mutationFn: () => api.deleteProject(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["projects-explicit"] });
      navigate("/projects", { replace: true });
    },
    onError: (err: Error) => toast(`Could not delete project: ${err.message}`, "error"),
  });

  if (isLoading) return <Spinner label="Loading project…" />;
  if (isError || !data)
    return (
      <div className="space-y-4">
        <BackLink />
        <ErrorState
          title="Could not load project"
          message={(error as Error)?.message}
          onRetry={() => refetch()}
        />
      </div>
    );

  const { project, sessions } = data;
  const isOwner =
    !user || project.owner_email == null || project.owner_email === user.email;

  return (
    <div className="space-y-5">
      <BackLink />

      <div className="rounded-xl border border-border bg-card p-5">
        <div className="flex flex-wrap items-start justify-between gap-3">
          {editing ? (
            <form
              className="flex flex-1 flex-col gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                const trimmed = name.trim();
                if (trimmed) {
                  const desc = description.trim();
                  rename.mutate({ name: trimmed, description: desc || undefined });
                }
              }}
            >
              <div className="flex items-center gap-2">
                <Input
                  aria-label="Project name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  className="flex-1"
                />
                <Button type="submit" size="sm" disabled={rename.isPending || !name.trim()}>
                  {rename.isPending ? "Saving…" : "Save"}
                </Button>
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    setName(project.name);
                    setDescription(project.description ?? "");
                    setEditing(false);
                  }}
                >
                  Cancel
                </Button>
              </div>
              <textarea
                aria-label="Project description"
                placeholder="Description (optional)…"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                rows={2}
                className="flex w-full rounded-md border border-input bg-card/60 px-3 py-2 text-sm shadow-xs transition-colors placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-50"
              />
            </form>
          ) : (
            <div className="flex flex-1 flex-wrap items-start justify-between gap-3">
              <div className="min-w-0 flex-1">
                <h1 className="text-xl font-semibold leading-snug tracking-tight">
                  {project.name}
                </h1>
                {project.description && (
                  <p className="mt-1 whitespace-pre-wrap text-sm text-muted-foreground">
                    {project.description}
                  </p>
                )}
              </div>
              {isOwner && (
                <div className="flex items-center gap-2">
                  <Button size="sm" variant="outline" onClick={() => setEditing(true)}>
                    Edit
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                    onClick={() => remove.mutate()}
                    disabled={remove.isPending}
                  >
                    Delete
                  </Button>
                </div>
              )}
            </div>
          )}
        </div>
        <div className="mt-3 text-xs text-muted-foreground">
          {sessions.length} session{sessions.length === 1 ? "" : "s"} · created{" "}
          {humanizeTime(project.created_at)}
        </div>
      </div>

      <div className="rounded-xl border border-border bg-card p-5">
        <h2 className="text-sm font-semibold text-foreground">Sessions</h2>
        {sessions.length === 0 ? (
          <p className="mt-3 text-xs text-muted-foreground">
            No sessions yet. Open a session and assign it to this project.
          </p>
        ) : (
          <ul className="mt-3 space-y-2">
            {sessions.map((s) => (
              <li key={s.id}>
                <Link
                  to={`/sessions/${encodeURIComponent(s.id)}`}
                  className="flex items-center justify-between gap-3 rounded-lg border border-border/70 px-3 py-2 text-sm transition-colors hover:bg-accent"
                >
                  <span className="truncate text-foreground">{s.title}</span>
                  <span className="flex shrink-0 items-center gap-2">
                    {s.git_branch && (
                      <Badge variant="branch" className="py-0">
                        ⎇ {s.git_branch}
                      </Badge>
                    )}
                    {s.project_path && (
                      <span
                        className="hidden font-mono text-xs text-muted-foreground sm:inline"
                        title={s.project_path}
                      >
                        {shortProject(s.project_path)}
                      </span>
                    )}
                    <VisibilityBadge visibility={s.visibility} />
                  </span>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </div>

      <ChatPanel parent={{ kind: "project", id: project.id }} />
    </div>
  );
}

function BackLink() {
  return (
    <Link
      to="/projects"
      className="inline-flex items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-foreground"
    >
      ← Back to projects
    </Link>
  );
}
