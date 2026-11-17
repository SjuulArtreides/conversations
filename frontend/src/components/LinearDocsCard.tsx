import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useSessionLinearSource } from "@/lib/sessionLinearSource";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";
import { Spinner } from "@/components/states";
import type { SessionLinearDoc } from "@/lib/types";
import { safeHref } from "@/lib/safeHref";

/** react-query cache key for a session's linked Linear refs. Shared so the
 *  card and any live-refresh nudge invalidate the same entry. */
export function sessionLinearDocsKey(sessionId: string) {
  return ["session-linear-docs", sessionId] as const;
}

/**
 * Card listing the Linear references (document / issue / project) auto-detected
 * in a session's messages, used by both the stored-session view
 * (`SessionDetail`) and the live-session view (`LiveSessionDetail`), rendered
 * directly below the Pull requests card. Uses
 * `GET /api/sessions/{id}/linear-docs` (the server authorizes a live id too).
 *
 * The Linear analogue of {@link PullRequestsCard}, but a strict subset: the
 * server holds no Linear credentials, so there is no status, no "Sync now", and
 * no comments — each row is just the entity link plus an Unlink button. Linear
 * URLs are auto-linked on ingest, so a session that mentions one shows it here
 * without any manual step.
 */
export function LinearDocsCard({ sessionId }: { sessionId: string }) {
  const source = useSessionLinearSource();
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const {
    data: docs,
    isLoading,
    error,
    refetch,
  } = useQuery({
    queryKey: source.key(sessionLinearDocsKey(sessionId)),
    queryFn: () => source.list(sessionId),
    // A live session may briefly 404 before its first message; don't spam.
    retry: false,
  });

  const unlink = useMutation({
    mutationFn: (url: string) => source.unlink(sessionId, url),
    onSuccess: () => {
      queryClient.invalidateQueries({
        queryKey: source.key(sessionLinearDocsKey(sessionId)),
      });
      toast("Linear reference removed");
    },
    onError: (err: Error) =>
      toast(`Could not remove reference: ${err.message}`, "error"),
  });

  return (
    <div className="flex max-h-[calc(100dvh-2rem)] flex-col rounded-xl border border-border bg-card p-5">
      <h2 className="text-sm font-semibold text-foreground">Linear</h2>

      {error && (
        <div role="alert" className="mt-3 text-xs text-destructive">
          {error.message}{" "}
          <Button variant="ghost" size="sm" onClick={() => void refetch()}>
            Retry
          </Button>
        </div>
      )}
      {isLoading ? (
        <Spinner label="Loading Linear references…" />
      ) : !docs || docs.length === 0 ? (
        <p className="mt-3 text-xs text-muted-foreground">
          No Linear references. Document, issue, and project links detected in
          chat messages appear here automatically.
        </p>
      ) : (
        <ul className="mt-3 min-h-0 space-y-3 overflow-y-auto">
          {docs.map((doc) => (
            <LinearRow
              key={doc.url}
              doc={doc}
              onUnlink={() => unlink.mutate(doc.url)}
              unlinking={unlink.isPending}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

/** One row in the Linear card. */
function LinearRow({
  doc,
  onUnlink,
  unlinking,
}: {
  doc: SessionLinearDoc;
  onUnlink: () => void;
  unlinking: boolean;
}) {
  return (
    <li
      className="rounded-lg border border-border/70 p-3"
      data-testid="linear-doc-row"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <LinearKindBadge kind={doc.kind} />
          <a
            href={safeHref(doc.url)}
            target="_blank"
            rel="noopener noreferrer"
            className="font-medium text-foreground hover:text-primary hover:underline"
          >
            {doc.identifier}
          </a>
          <span className="text-[0.65rem] text-muted-foreground">
            {doc.workspace}
          </span>
        </div>
        <Button
          size="sm"
          variant="ghost"
          className="shrink-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
          onClick={onUnlink}
          disabled={unlinking}
        >
          Unlink
        </Button>
      </div>
    </li>
  );
}

/** Subtle kind badge: document / issue / project. Falls back to the raw kind. */
function LinearKindBadge({ kind }: { kind: string }) {
  const styles: Record<string, string> = {
    document: "bg-blue-500/15 text-blue-400",
    issue: "bg-amber-500/15 text-amber-400",
    project: "bg-purple-500/15 text-purple-400",
  };
  const cls = styles[kind] ?? "bg-muted text-muted-foreground";
  return (
    <Badge
      className={`border-transparent py-0 capitalize ${cls}`}
      data-testid="linear-kind-badge"
    >
      {kind}
    </Badge>
  );
}
