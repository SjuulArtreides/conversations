import { privateProjects } from "@/lib/privateProjects";
import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { LIVE_SESSIONS_QUERY_KEY } from "@/lib/useLiveSessions";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";
import { useToast } from "./ui/toast";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Button } from "./ui/button";

interface ProjectAssignProps {
  sessionId: string;
  hostId?: string;
  /** The session's current explicit project, or null. */
  projectId: string | null;
  onSaved?: () => void;
  onCancel?: () => void;
}

/** Choose a project, then save the assignment explicitly. */
export function ProjectAssign({
  sessionId,
  hostId,
  projectId,
  onSaved,
  onCancel,
}: ProjectAssignProps) {
  const [draft, setSelection] = React.useState<string | null | undefined>(
    undefined,
  );
  const selection = draft === undefined ? projectId : draft;
  const [newName, setNewName] = React.useState<string | null>(null);
  const inputId = React.useId();
  const listId = `${inputId}-list`;
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const projectsQuery = useQuery({
    queryKey: hostId ? ["private", hostId, "projects"] : ["projects-explicit"],
    queryFn: () => (hostId ? privateProjects.list(hostId) : api.projects()),
  });

  // Shared query-key invalidation: the session detail carries project_id;
  // project details carry the member list and session_count — refresh both.
  const invalidate = React.useCallback(
    () =>
      hostId
        ? queryClient.invalidateQueries({ queryKey: ["private", hostId] })
        : Promise.all([
            queryClient.invalidateQueries({ queryKey: ["session", sessionId] }),
            queryClient.invalidateQueries({
              queryKey: ["local-session-project", sessionId],
            }),
            queryClient.invalidateQueries({ queryKey: ["sessions"] }),
            queryClient.invalidateQueries({
              queryKey: LIVE_SESSIONS_QUERY_KEY,
            }),
            queryClient.invalidateQueries({ queryKey: ["projects-explicit"] }),
            queryClient.invalidateQueries({ queryKey: ["project"] }),
          ]),
    [queryClient, sessionId, hostId],
  );

  const assign = useMutation({
    mutationFn: (next: string | null) =>
      hostId
        ? privateProjects.assign(hostId, sessionId, next)
        : api.setSessionProject(sessionId, next),
    onSuccess: async () => {
      await invalidate();
      setSelection(undefined);
      onSaved?.();
    },
    onError: (err: Error) =>
      toast(`Could not update project: ${err.message}`, "error"),
  });

  const createAndAssign = useMutation({
    mutationFn: async (name: string) => {
      const project = await (hostId
        ? privateProjects.create(hostId, name)
        : api.createProject(name));
      queryClient.invalidateQueries({
        queryKey: hostId
          ? ["private", hostId, "projects"]
          : ["projects-explicit"],
      });
      setSelection(project.id);
      setNewName(null);
      await (hostId
        ? privateProjects.assign(hostId, sessionId, project.id)
        : api.setSessionProject(sessionId, project.id));
      return project;
    },
    onSuccess: async () => {
      await invalidate();
      setSelection(undefined);
      onSaved?.();
    },
    onError: (err: Error) =>
      toast(`Could not create project: ${err.message}`, "error"),
  });

  const projects = projectsQuery.data ?? [];

  const [query, setQuery] = React.useState("");
  const [open, setOpen] = React.useState(false);
  const [pendingName, setPendingName] = React.useState<string | null>(null);
  const blurCloseTimerRef = React.useRef<number | null>(null);

  React.useEffect(() => {
    return () => {
      if (blurCloseTimerRef.current != null) {
        window.clearTimeout(blurCloseTimerRef.current);
      }
    };
  }, []);

  const currentProject = projects.find((p) => p.id === selection) ?? null;

  const trimmed = query.trim();
  const filtered = trimmed
    ? projects.filter((p) =>
        p.name.toLowerCase().includes(trimmed.toLowerCase()),
      )
    : projects;
  const exactMatch = projects.find(
    (p) => p.name.toLowerCase() === trimmed.toLowerCase(),
  );

  const saving = assign.isPending || createAndAssign.isPending;

  const select = (id: string | null) => {
    setOpen(false);
    setQuery("");
    setSelection(id);
    setNewName(null);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") {
      e.preventDefault();
      if (!trimmed) return;
      if (exactMatch) {
        select(exactMatch.id);
        return;
      }
      // No matching project — confirm creating it.
      setPendingName(trimmed);
    } else if (e.key === "Escape") {
      setOpen(false);
    }
  };

  const confirmCreate = () => {
    if (pendingName == null) return;
    const name = pendingName;
    setPendingName(null);
    setOpen(false);
    setQuery("");
    setNewName(name);
  };

  return (
    <div className="flex flex-wrap items-start gap-3 rounded-xl border border-border bg-card p-4">
      <label
        htmlFor={inputId}
        className="mt-2 text-sm font-semibold text-foreground"
      >
        Project
      </label>

      <div className="relative min-w-56 flex-1">
        <input
          id={inputId}
          type="text"
          role="combobox"
          aria-expanded={open}
          aria-autocomplete="list"
          aria-controls={listId}
          aria-label="Assign to project"
          autoComplete="off"
          placeholder={newName ?? currentProject?.name ?? "No project"}
          value={query}
          disabled={saving || projectsQuery.isLoading}
          onChange={(e) => {
            setQuery(e.target.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          onBlur={() => {
            // Delay so option mousedown handlers run before the list unmounts.
            if (blurCloseTimerRef.current != null) {
              window.clearTimeout(blurCloseTimerRef.current);
            }
            blurCloseTimerRef.current = window.setTimeout(() => {
              blurCloseTimerRef.current = null;
              setOpen(false);
            }, 120);
          }}
          onKeyDown={onKeyDown}
          className={cn(
            "h-9 w-full rounded-md border border-input bg-card/60 px-3 text-sm shadow-xs transition-colors",
            "placeholder:text-muted-foreground",
            "focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background",
            "disabled:cursor-not-allowed disabled:opacity-50",
          )}
        />

        {open && (
          <ul
            id={listId}
            role="listbox"
            className="absolute z-20 mt-1 max-h-60 w-full overflow-auto rounded-md border border-border bg-card py-1 text-sm shadow-lg"
          >
            <li role="option" aria-selected={selection == null}>
              <button
                type="button"
                className="block w-full px-3 py-1.5 text-left text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => select(null)}
              >
                No project
              </button>
            </li>
            {filtered.map((p) => (
              <li key={p.id} role="option" aria-selected={p.id === selection}>
                <button
                  type="button"
                  className={cn(
                    "block w-full truncate px-3 py-1.5 text-left text-foreground transition-colors hover:bg-accent",
                    p.id === selection && "bg-accent/60",
                  )}
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => select(p.id)}
                >
                  {p.name}
                </button>
              </li>
            ))}
            {trimmed && !exactMatch && (
              <li role="option" aria-selected={false}>
                <button
                  type="button"
                  className="block w-full truncate px-3 py-1.5 text-left text-foreground transition-colors hover:bg-accent"
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => setPendingName(trimmed)}
                >
                  Create “{trimmed}”…
                </button>
              </li>
            )}
          </ul>
        )}
      </div>

      <Button
        type="button"
        disabled={
          saving ||
          projectsQuery.isLoading ||
          (newName == null && selection === projectId)
        }
        onClick={() =>
          newName != null
            ? createAndAssign.mutate(newName)
            : assign.mutate(selection)
        }
      >
        {saving ? "Saving…" : "Save"}
      </Button>
      {onCancel && (
        <Button
          type="button"
          variant="outline"
          disabled={saving}
          onClick={onCancel}
        >
          Cancel
        </Button>
      )}

      {saving && (
        <span className="mt-2 text-xs text-muted-foreground">Saving…</span>
      )}

      <Dialog
        open={pendingName != null}
        onOpenChange={(o) => {
          if (!o) setPendingName(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Create project?</DialogTitle>
            <DialogDescription>
              No project matches “{pendingName}”. Select it for this session?
              The project is created when you save.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="ghost"
              onClick={() => setPendingName(null)}
            >
              Cancel
            </Button>
            <Button type="button" onClick={confirmCreate}>
              Select new project
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
