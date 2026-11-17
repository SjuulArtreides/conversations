import { PrReviewIcon } from "@/components/PrReviewIcon";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useSessionPrSource } from "@/lib/sessionPrSource";
import { sessionPrsKey } from "@/components/ChatPanel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";
import { Spinner } from "@/components/states";
import { fullTimestamp, humanizeTime } from "@/lib/utils";
import { prAnchorId } from "@/lib/prUrls";
import type { SessionPr } from "@/lib/types";
import { safeHref } from "@/lib/safeHref";

/**
 * Card listing linked PRs for a session, used by both the stored-session view
 * (`SessionDetail`) and the live-session view (`LiveSessionDetail`). Uses
 * `GET /api/sessions/{id}/prs` (the server authorizes a live id too); each row
 * shows owner/repo#number, the PR title (from status), a state badge, and an
 * Unlink button. "Sync now" relays a sync request to the viewer's attached CLI.
 *
 * Detected PR URLs are auto-linked on ingest, so a session that mentions a PR
 * shows it here without any manual step.
 */
export function PullRequestsCard({ sessionId }: { sessionId: string }) {
  const source = useSessionPrSource();
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const {
    data: prs,
    isLoading,
    error,
    refetch,
  } = useQuery({
    queryKey: source.key(sessionPrsKey(sessionId)),
    queryFn: () => source.listSessionPrs(sessionId),
    // A live session may briefly 404 before its first message; don't spam.
    retry: false,
    // Pick up host-driven cache changes (the daemon re-fetches PR metadata
    // on its own per-target backoff) without forcing the user to reload.
    // Pauses when the tab is backgrounded. The daemon's minimum poll floor
    // is 60s so refetching every 30s keeps us comfortably within one tick.
    refetchInterval: (query) =>
      query.state.status === "success"
        ? (source.refetchInterval ?? false)
        : false,
    refetchIntervalInBackground: false,
  });

  const unlink = useMutation({
    mutationFn: ({
      owner,
      repo,
      number,
    }: {
      owner: string;
      repo: string;
      number: number;
    }) => source.unlinkSessionPr(sessionId, owner, repo, number),
    onSuccess: () => {
      queryClient.invalidateQueries({
        queryKey: source.key(sessionPrsKey(sessionId)),
      });
      toast("PR unlinked");
    },
    onError: (err: Error) =>
      toast(`Could not unlink PR: ${err.message}`, "error"),
  });

  // "Sync now": relay a sync request to the viewer's attached CLI (gh stays on
  // the CLI — the server holds no GitHub creds). On delivery, the CLI pushes
  // fresh status + comments; we re-fetch the cached list shortly after.
  const sync = useMutation({
    mutationFn: () => source.syncPrs(sessionId),
    onSuccess: ({ delivered }) => {
      if (delivered) {
        toast("Syncing PRs…");
        // The CLI fetches via gh then pushes back; give it a moment, then
        // refresh the cached list. (v1 has no sync-done signal — see plan.)
        window.setTimeout(
          () =>
            queryClient.invalidateQueries({
              queryKey: source.key(sessionPrsKey(sessionId)),
            }),
          2500,
        );
      } else {
        toast(
          "No sync agent connected — run `conversations host`",
          "error",
        );
      }
    },
    onError: (err: Error) =>
      toast(`Could not sync PRs: ${err.message}`, "error"),
  });

  const hasPrs = !!prs && prs.length > 0;

  return (
    <div className="flex max-h-[calc(100dvh-2rem)] flex-col rounded-xl border border-border bg-card p-5">
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-sm font-semibold text-foreground">Pull requests</h2>
        {hasPrs && (
          <Button
            size="sm"
            variant="outline"
            className="shrink-0"
            onClick={() => sync.mutate()}
            disabled={sync.isPending}
            data-testid="pr-sync-now"
          >
            {sync.isPending ? "Syncing…" : "Sync now"}
          </Button>
        )}
      </div>

      {error && (
        <p role="alert" className="mt-3 text-xs text-destructive">
          Could not load pull requests: {(error as Error).message}{" "}
          <button
            type="button"
            className="underline"
            onClick={() => void refetch()}
          >
            Retry
          </button>
        </p>
      )}
      {isLoading ? (
        <Spinner label="Loading pull requests…" />
      ) : !prs || prs.length === 0 ? (
        <p className="mt-3 text-xs text-muted-foreground">
          No pull requests linked. PR URLs detected in chat messages can be
          linked here.
        </p>
      ) : (
        <ul className="mt-3 min-h-0 space-y-3 overflow-y-auto">
          {prs.map((pr) => (
            <PrRow
              key={pr.url}
              pr={pr}
              onUnlink={() =>
                unlink.mutate({
                  owner: pr.owner,
                  repo: pr.repo,
                  number: pr.number,
                })
              }
              unlinking={unlink.isPending}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

/** One row in the Pull Requests card. */
function PrRow({
  pr,
  onUnlink,
  unlinking,
}: {
  pr: SessionPr;
  onUnlink: () => void;
  unlinking: boolean;
}) {
  return (
    <li
      id={prAnchorId(pr.owner, pr.repo, pr.number)}
      // scroll-mt keeps the row clear of any sticky chrome when scrolled to via
      // an inline PR highlight; the transition lets the jump flash fade out.
      className="scroll-mt-20 rounded-lg border border-border/70 p-3 transition-[box-shadow,background-color] duration-500"
      data-testid="pr-row"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <a
            href={safeHref(pr.url)}
            target="_blank"
            rel="noopener noreferrer"
            className="font-medium text-foreground hover:text-primary hover:underline"
          >
            {pr.owner}/{pr.repo}#{pr.number}
          </a>
          <PrStatusBadge status={pr.status} />
          {pr.detected && (
            <Badge
              className="border-transparent bg-muted py-0 text-[0.65rem] font-normal text-muted-foreground"
              data-testid="pr-auto-detected"
            >
              auto-detected
            </Badge>
          )}
          {pr.status && pr.status_fetched_at && (
            <span
              className="text-[0.65rem] text-muted-foreground"
              data-testid="pr-status-synced"
              title={fullTimestamp(pr.status_fetched_at)}
            >
              synced {humanizeTime(pr.status_fetched_at)}
            </span>
          )}
        </div>
        <PrReviewIcon status={pr.status} />
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
      {pr.status?.title && (
        <p
          className="mt-1 truncate text-xs text-muted-foreground"
          title={pr.status.title}
        >
          {pr.status.title}
        </p>
      )}
      {!pr.status && (
        <p
          className="mt-1 text-xs text-muted-foreground"
          data-testid="pr-status-not-synced"
        >
          status not synced — hit{" "}
          <span className="font-medium text-foreground">Sync now</span> (needs a
          connected{" "}
          <code className="rounded bg-muted px-1 py-px text-[0.7em]">
            conversations host
          </code>
          )
        </p>
      )}
    </li>
  );
}

/** Status badge derived from PR state, merged, and draft flags. */
function PrStatusBadge({ status }: { status: SessionPr["status"] }) {
  if (!status) return null;

  // `state` is gh's raw value (`OPEN` / `CLOSED` / `MERGED`), so normalize case
  // before comparing — a verbatim `=== "open"` would mislabel every open PR.
  const state = status.state?.toUpperCase();

  if (status.merged) {
    return (
      <Badge
        className="border-transparent bg-purple-500/15 py-0 text-purple-400"
        data-testid="pr-status-badge"
      >
        Merged
      </Badge>
    );
  }
  if (status.draft) {
    return (
      <Badge
        className="border-transparent bg-muted py-0 text-muted-foreground"
        data-testid="pr-status-badge"
      >
        Draft
      </Badge>
    );
  }
  if (state === "OPEN") {
    return (
      <Badge
        className="border-transparent bg-green-500/15 py-0 text-green-400"
        data-testid="pr-status-badge"
      >
        Open
      </Badge>
    );
  }
  // state === "CLOSED" and not merged
  return (
    <Badge
      className="border-transparent bg-red-500/15 py-0 text-red-400"
      data-testid="pr-status-badge"
    >
      Closed
    </Badge>
  );
}
