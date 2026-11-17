import * as React from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useRedirectOnUnauthorized } from "@/lib/useAuth";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { EmptyState, ErrorState, Spinner } from "@/components/states";
import { useToast } from "@/components/ui/toast";
import { humanizeTime } from "@/lib/utils";

export function ProjectList() {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const [name, setName] = React.useState("");
  const [description, setDescription] = React.useState("");

  const { data, isLoading, isError, error, refetch } = useQuery({
    queryKey: ["projects-explicit"],
    queryFn: api.projects,
  });

  useRedirectOnUnauthorized(error);

  const create = useMutation({
    mutationFn: (input: { name: string; description?: string }) =>
      api.createProject(input.name, input.description),
    onSuccess: () => {
      setName("");
      setDescription("");
      queryClient.invalidateQueries({ queryKey: ["projects-explicit"] });
    },
    onError: (err: Error) => toast(`Could not create project: ${err.message}`, "error"),
  });

  return (
    <div className="space-y-5">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">Projects</h1>
        <p className="text-sm text-muted-foreground">
          Group sessions and chat about them as a unit.
        </p>
      </div>

      <form
        className="flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const trimmed = name.trim();
          if (trimmed) {
            const desc = description.trim();
            create.mutate({ name: trimmed, description: desc || undefined });
          }
        }}
      >
        <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
          <Input
            aria-label="New project name"
            placeholder="New project name…"
            value={name}
            onChange={(e) => setName(e.target.value)}
            className="flex-1"
          />
          <Button type="submit" disabled={create.isPending || !name.trim()}>
            {create.isPending ? "Creating…" : "Create project"}
          </Button>
        </div>
        <textarea
          aria-label="New project description"
          placeholder="Description (optional)…"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={2}
          className="flex w-full rounded-md border border-input bg-card/60 px-3 py-2 text-sm shadow-xs transition-colors placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-50"
        />
      </form>

      {isLoading ? (
        <Spinner label="Loading projects…" />
      ) : isError ? (
        <ErrorState
          message={(error as Error)?.message}
          onRetry={() => refetch()}
        />
      ) : (data ?? []).length === 0 ? (
        <EmptyState
          title="No projects yet"
          message="Create a project above, then assign sessions to it."
        />
      ) : (
        <ul className="grid items-start gap-3 sm:grid-cols-2">
          {data!.map((p) => (
            <li key={p.id}>
              <Link
                to={`/projects/${encodeURIComponent(p.id)}`}
                className="block rounded-xl border border-border bg-card p-4 transition-colors hover:bg-accent"
              >
                <div className="flex items-center justify-between gap-3">
                  <span className="truncate font-medium text-foreground">
                    {p.name}
                  </span>
                  <span className="shrink-0 text-xs text-muted-foreground">
                    {p.session_count} session{p.session_count === 1 ? "" : "s"}
                  </span>
                </div>
                <div className="mt-1 text-xs text-muted-foreground">
                  updated {humanizeTime(p.updated_at)}
                </div>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
