import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import type { RepoLink, RepoLinkInput, Session } from "@/lib/types";
import { api } from "@/lib/api";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { useToast } from "./ui/toast";

interface RepoDialogProps {
  session: Pick<Session, "id" | "title" | "repos">;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** Drop the per-user `paths` to get the editable canonical shape. */
function toInput(r: RepoLink): RepoLinkInput {
  return {
    repo: r.repo,
    remote_url: r.remote_url,
    branch: r.branch,
    pr_url: r.pr_url,
  };
}

function blankRow(): RepoLinkInput {
  return { repo: "", remote_url: null, branch: "", pr_url: null };
}

/** Trim a text field down to `string | null` (empty → null). */
function clean(v: string): string | null {
  const t = v.trim();
  return t === "" ? null : t;
}

export function RepoDialog({ session, open, onOpenChange }: RepoDialogProps) {
  const [rows, setRows] = React.useState<RepoLinkInput[]>(
    session.repos.map(toInput),
  );
  const queryClient = useQueryClient();
  const { toast } = useToast();

  // Reset to the session's current repos whenever the dialog (re)opens.
  React.useEffect(() => {
    if (open) setRows(session.repos.map(toInput));
  }, [open, session.repos]);

  const update = (i: number, patch: Partial<RepoLinkInput>) =>
    setRows((prev) => prev.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const remove = (i: number) =>
    setRows((prev) => prev.filter((_, j) => j !== i));
  const add = () => setRows((prev) => [...prev, blankRow()]);

  // Only rows with both a repo name and a branch are valid links.
  const cleaned: RepoLinkInput[] = rows
    .map((r) => ({
      repo: r.repo.trim(),
      branch: r.branch.trim(),
      remote_url: r.remote_url,
      pr_url: r.pr_url,
    }))
    .filter((r) => r.repo !== "" && r.branch !== "");

  const mutation = useMutation({
    mutationFn: () => api.setRepos(session.id, cleaned),
    onSuccess: (repos) => {
      queryClient.setQueryData<Session>(["session", session.id], (prev) =>
        prev ? { ...prev, repos } : prev,
      );
      queryClient.invalidateQueries({ queryKey: ["session", session.id] });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      toast("Repositories updated");
      onOpenChange(false);
    },
    onError: (err: Error) => {
      toast(`Failed to update repositories: ${err.message}`, "error");
    },
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby="repos-desc" className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>Edit repositories</DialogTitle>
          <DialogDescription id="repos-desc">
            Which repos and branches “{session.title}” touched. Per-user local
            paths are kept automatically and aren’t editable here.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          {rows.length === 0 && (
            <p className="text-sm text-muted-foreground">
              No repositories linked yet.
            </p>
          )}
          {rows.map((r, i) => (
            <div
              key={i}
              className="grid grid-cols-1 gap-2 rounded-lg border border-border p-3 sm:grid-cols-2"
            >
              <label className="text-xs font-medium text-muted-foreground">
                Repo
                <Input
                  value={r.repo}
                  onChange={(e) => update(i, { repo: e.target.value })}
                  placeholder="my-service"
                  className="mt-1"
                />
              </label>
              <label className="text-xs font-medium text-muted-foreground">
                Branch
                <Input
                  value={r.branch}
                  onChange={(e) => update(i, { branch: e.target.value })}
                  placeholder="main"
                  className="mt-1"
                />
              </label>
              <label className="text-xs font-medium text-muted-foreground">
                Remote URL
                <Input
                  value={r.remote_url ?? ""}
                  onChange={(e) =>
                    update(i, { remote_url: clean(e.target.value) })
                  }
                  placeholder="git@github.com:org/my-service.git"
                  className="mt-1"
                />
              </label>
              <label className="text-xs font-medium text-muted-foreground">
                PR URL
                <Input
                  value={r.pr_url ?? ""}
                  onChange={(e) => update(i, { pr_url: clean(e.target.value) })}
                  placeholder="https://github.com/org/my-service/pull/42"
                  className="mt-1"
                />
              </label>
              <div className="sm:col-span-2 flex justify-end">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => remove(i)}
                  type="button"
                >
                  Remove
                </Button>
              </div>
            </div>
          ))}

          <Button variant="outline" size="sm" onClick={add} type="button">
            + Add repository
          </Button>
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={mutation.isPending}
          >
            Cancel
          </Button>
          <Button
            onClick={() => mutation.mutate()}
            disabled={mutation.isPending}
          >
            {mutation.isPending ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
