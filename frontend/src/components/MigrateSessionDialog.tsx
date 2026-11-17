import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { isLocalHost, localRequest, LOCAL_HOST_ID } from "@/lib/localHost";
import { api } from "@/lib/api";
import type { UserSettings } from "@/lib/types";
import { isPinned, usePairedHosts } from "@/lib/hostPairing";
import {
  migrateSession,
  MigrationResumeError,
  MigrationCopyUncertainError,
} from "@/lib/sessionMigration";
import type { SessionEntry } from "@/lib/privateChannel";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Button } from "./ui/button";

export function MigrateSessionDialog({
  session,
  hostId,
  onOpenChange,
}: {
  session: SessionEntry;
  hostId: string;
  onOpenChange: (open: boolean) => void;
}) {
  const { hosts } = usePairedHosts();
  const local = isLocalHost();
  const settings = useQuery({
    queryKey: local ? ["local-user-settings"] : ["user-settings"],
    queryFn: () =>
      local
        ? localRequest<UserSettings>("/api/local/preferences")
        : api.getUserSettings(),
    staleTime: 30_000,
  });
  const repositories = settings.data?.container_paths ?? [];
  const [repository, setRepository] = useState("");
  const destinations = [
    ...hosts.filter((host) => host.id !== hostId && isPinned(host)),
    ...(isLocalHost() && hostId !== LOCAL_HOST_ID
      ? [{ id: LOCAL_HOST_ID, label: "This host" }]
      : []),
  ];
  const [target, setTarget] = useState("");
  const [cwd, setCwd] = useState(session.cwd);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const navigate = useNavigate();
  const queries = useQueryClient();
  const href = (id: string) =>
    `/private/${encodeURIComponent(id)}?host=${encodeURIComponent(target)}`;
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (busy || !destinations.some((host) => host.id === target)) return;
    setBusy(true);
    setError(null);
    setProgress("Connecting to hosts…");
    try {
      const id = await migrateSession({
        sourceHost: hostId,
        sourceSession: session.id,
        destinationHost: target,
        cwd,
        onProgress: (copied, total) =>
          setProgress(
            copied === total
              ? "Resuming on destination…"
              : `Copying… ${Math.floor((copied / total) * 100)}%`,
          ),
      });
      await queries.invalidateQueries({ queryKey: ["private", target] });
      onOpenChange(false);
      navigate(href(id));
    } catch (cause) {
      if (
        cause instanceof MigrationResumeError ||
        cause instanceof MigrationCopyUncertainError
      ) {
        setCopiedId(cause.destination);
        await queries.invalidateQueries({ queryKey: ["private", target] });
      }
      setError(cause instanceof Error ? cause.message : "Migration failed.");
    } finally {
      setBusy(false);
      setProgress(null);
    }
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!busy) onOpenChange(open);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Migrate session</DialogTitle>
          <DialogDescription>
            Copy this session and resume it on another host. The source session
            stays available. Repository files and uncommitted changes are not
            copied.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={(event) => void submit(event)} className="space-y-4">
          <label className="block text-sm">
            Destination host
            <select
              aria-label="Destination host"
              value={target}
              disabled={busy || !!copiedId}
              onChange={(event) => setTarget(event.target.value)}
              className="mt-1 block w-full rounded border border-border bg-background p-2"
            >
              <option value="">Choose a host</option>
              {destinations.map((host) => (
                <option key={host.id} value={host.id}>
                  {host.label}
                </option>
              ))}
            </select>
          </label>
          {!destinations.length && (
            <p className="text-sm text-muted-foreground">
              Pair another host to migrate this session.
            </p>
          )}
          <label className="block text-sm">
            Destination folder
            <select
              aria-label="Destination folder"
              value={repository}
              disabled={busy || !!copiedId}
              onChange={(event) => {
                setRepository(event.target.value);
                if (event.target.value) setCwd(event.target.value);
              }}
              className="mt-1 block w-full rounded border border-border bg-background p-2"
            >
              <option value="">Custom folder</option>
              {repositories.map((path) => (
                <option key={path} value={path}>
                  {path}
                </option>
              ))}
            </select>
          </label>
          {settings.isPending && (
            <p className="text-sm text-muted-foreground">
              Loading saved repositories… You can enter a folder below.
            </p>
          )}
          {settings.isError && (
            <p className="text-sm text-muted-foreground">
              Could not load saved repositories. Enter a folder below.
            </p>
          )}
          {!repository && (
            <label className="block text-sm">
              Folder path
              <input
                aria-label="Folder path"
                value={cwd}
                disabled={busy || !!copiedId}
                onChange={(event) => setCwd(event.target.value)}
                className="mt-1 block w-full rounded border border-border bg-background p-2"
                required
              />
            </label>
          )}
          <p className="text-sm text-muted-foreground">
            Saved folders come from Settings → Repositories. Use an existing
            project folder on the destination. That host checks your access and
            uses its own agent settings.
          </p>
          {progress && <p role="status">{progress}</p>}
          {error && <p role="alert">{error}</p>}
          {copiedId && (
            <Link
              to={href(copiedId)}
              onClick={() => onOpenChange(false)}
              className="text-sm underline"
            >
              Open destination session
            </Link>
          )}
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => onOpenChange(false)}
            >
              Close
            </Button>
            <Button
              type="submit"
              disabled={busy || !target || !cwd.trim() || !!copiedId}
            >
              {busy ? "Migrating…" : "Copy and resume"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
