import { Button } from "@/components/ui/button";
import { SessionDetailHeader } from "@/components/SessionDetailHeader";
import { RenameSessionDialog } from "@/components/RenameSessionDialog";
import { LiveShareDialog } from "@/components/LiveShareDialog";
import { LiveBadge } from "@/components/LiveBadge";
import { SessionContext } from "@/components/SessionContext";
import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router-dom";
import { sessionPrsKey } from "@/components/ChatPanel";
import { MessageBubble } from "@/components/MessageBubble";
import { LiveTabs } from "@/components/LiveTabs";
import { PrActivityPanel } from "@/components/PrActivityPanel";
import { PullRequestsCard } from "@/components/PullRequestsCard";
import {
  LinearDocsCard,
  sessionLinearDocsKey,
} from "@/components/LinearDocsCard";
import { PrDetectProvider, usePrDetectValue } from "@/components/PrDetect";
import { deriveRepoContextFromTexts } from "@/lib/prRefs";
import {
  RestartSessionButton,
  StartNewSessionButton,
  useStartNewSession,
  useEndLiveSession,
  useDismissLiveSession,
} from "@/components/HostActions";
import { ScrollNav } from "@/components/ScrollNav";
import { ErrorState } from "@/components/states";
import { api } from "@/lib/api";
import { buildToolResultMap, filterRenderedMessages } from "@/lib/toolResults";
import { useLiveStream } from "@/lib/useLiveStream";
import { useLiveSessions } from "@/lib/useLiveSessions";
import { useLiveSwitchHotkey } from "@/lib/useLiveSwitchHotkey";
import { useStickyScroll } from "@/lib/useStickyScroll";
import {
  latestPrCommentRevision,
  mergePrComments,
  prCommentPreviewsKey,
  prCommentsKey,
} from "@/lib/prCommentCache";
import type { PrComment } from "@/lib/types";

export function LiveSessionDetail() {
  const { id = "" } = useParams();
  const queryClient = useQueryClient();
  // The server pushes a `prs_linked` frame when it auto-detects & links a new
  // PR mid-session; refetch the shared PR list so the page card AND the
  // fullscreen terminal's PR rail (same `sessionPrsKey` cache) update live.
  const onPrsLinked = React.useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: sessionPrsKey(id) });
  }, [queryClient, id]);
  // Same nudge for auto-detected Linear refs (the card below the PRs).
  const onLinearDocsLinked = React.useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: sessionLinearDocsKey(id) });
  }, [queryClient, id]);
  const onPrCommentsChanged = React.useCallback(() => {
    const key = prCommentsKey(id);
    const current = queryClient.getQueryData<PrComment[]>(key);
    if (current) {
      const since = latestPrCommentRevision(current);
      void api
        .listPrComments(id, since)
        .then((changed) => {
          queryClient.setQueryData<PrComment[]>(key, (previous) =>
            previous ? mergePrComments(previous, changed) : previous,
          );
        })
        .catch(() => {
          // The panel's incremental poll will retry after a transient failure.
        });
    }
    void queryClient.invalidateQueries({
      queryKey: prCommentPreviewsKey(id),
    });
  }, [queryClient, id]);
  // The durable HTML transcript ("chat") starts collapsed for every agent. Its
  // message bodies and reply composer belong to it: while collapsed the stream
  // carries only the message outline (count + latest role).
  // Every session switch collapses it, including a return to a session that
  // was open. The reset happens during render, before any body request starts.
  const [transcriptOpen, setTranscriptOpen] = React.useState(false);
  const [transcriptId, setTranscriptId] = React.useState(id);
  if (transcriptId !== id) {
    setTranscriptId(id);
    setTranscriptOpen(false);
  }
  const { messages, outline, status } = useLiveStream(
    id,
    onPrsLinked,
    onLinearDocsLinked,
    onPrCommentsChanged,
    { bodies: transcriptOpen },
  );
  // Title comes from the live-sessions poll (the stream itself carries only
  // messages); it is carried by the active tab in the compact row.
  const { list } = useLiveSessions();
  const meta = list.find((s) => s.id === id);
  const title = meta?.title ?? "Live session";
  const [shareOpen, setShareOpen] = React.useState(false);
  const [renameOpen, setRenameOpen] = React.useState(false);

  // Tabs, shortcuts, and the end-session fallback share the same order.
  const orderedSessions = list;

  // tmux-style `Ctrl-A` prefix: `<n>` switches between the viewer's live
  // sessions, `c` asks the CLI host for a new one (and attaches to it once it
  // registers — preserving fullscreen).
  const { start: startNewSession } = useStartNewSession();
  const { armed: switchArmed } = useLiveSwitchHotkey(orderedSessions, id, {
    onCreate: startNewSession,
  });

  // The tab ✕ handles per-tab end/dismiss; the compact-row Actions menu
  // covers the current session. These hooks remain so the ✕ and hotkeys
  // keep sharing one queue.
  const { end: endLive, endingId } = useEndLiveSession(orderedSessions, id);
  const { dismiss: dismissLive, dismissingId } = useDismissLiveSession(
    orderedSessions,
    id,
  );
  const ending = endingId === id;
  const dismissing = dismissingId === id;

  const toolResults = buildToolResultMap(messages);
  const rendered = filterRenderedMessages(messages);
  // Sticky-scroll: keep the view pinned to the bottom while messages stream in,
  // but don't yank the user if they've scrolled up to read history.
  const {
    atTop,
    atBottom,
    scrollToTop,
    scrollToBottom,
    stickToBottomIfPinned,
  } = useStickyScroll();

  // Don't yank the view to the bottom when the page first opens — land at the
  // top and let the reader scroll. We skip the initial content load (the first
  // render with messages) and only auto-scroll on messages that arrive AFTER,
  // and only if the reader is currently pinned to the bottom. Reset per id so a
  // session switch also opens at the top.
  const sawInitialContentRef = React.useRef(false);
  React.useEffect(() => {
    sawInitialContentRef.current = false;
  }, [id]);
  React.useEffect(() => {
    if (outline.count === 0) return;
    if (!sawInitialContentRef.current) {
      sawInitialContentRef.current = true;
      return;
    }
    stickToBottomIfPinned();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [outline.count]);

  // The central terminal, reply composer and text mirror were removed with the
  // central live mirror: this page is a read-only transcript view. Terminal
  // control of a session lives on its own host, reached over the signed
  // private channel (the `/private` page).
  const isLive = status.kind !== "complete" && status.kind !== "error";

  // In-message PR detection: resolve a single repo context from full PR URLs
  // anywhere in the transcript, then let the message/tool renderers highlight
  // detected references and offer to link them. Author-only "Add" affordance.
  // A collapsed transcript has no bodies, so fall back to the linked PRs (the
  // same cached query the PR card uses) for the PR activity panel.
  const { data: linkedPrs } = useQuery({
    queryKey: sessionPrsKey(id),
    queryFn: () => api.listSessionPrs(id),
    retry: false,
    // Match PullRequestsCard: pick up host-driven cache refreshes so the
    // fullscreen PR rail stays current without a browser reload.
    refetchInterval: 30_000,
    refetchIntervalInBackground: false,
  });
  const derivedRepoCtx = React.useMemo(
    () =>
      deriveRepoContextFromTexts(
        messages.length > 0
          ? messages.map((m) => m.text)
          : (linkedPrs ?? []).map((pr) => pr.url),
      ),
    [messages, linkedPrs],
  );
  const repoCtxRef = React.useRef(derivedRepoCtx);
  if (
    repoCtxRef.current?.owner !== derivedRepoCtx?.owner ||
    repoCtxRef.current?.repo !== derivedRepoCtx?.repo
  ) {
    repoCtxRef.current = derivedRepoCtx;
  }
  const repoCtx = repoCtxRef.current;
  const prDetect = usePrDetectValue({
    sessionId: id,
    repoCtx,
    canAdd: meta?.viewer_is_author ?? false,
  });

  return (
    <PrDetectProvider value={prDetect}>
      <div className="@container space-y-5">
        <BackLink />

        <LiveTabs sessions={orderedSessions} currentId={id} />

        {switchArmed && (
          <div
            role="status"
            className="fixed bottom-6 left-1/2 z-70 -translate-x-1/2 rounded-lg border border-emerald-500/40 bg-emerald-500/15 px-4 py-2 text-sm font-medium text-emerald-200 shadow-lg"
          >
            Ctrl-A —{" "}
            {list.length >= 2 && (
              <>
                <kbd>1</kbd>–<kbd>{Math.min(list.length, 9)}</kbd> switch ·{" "}
              </>
            )}
            <kbd>c</kbd> new session
          </div>
        )}

        <>
          <SessionDetailHeader
              title={title}
              project={meta?.project_path}
              badges={
                <>
                  <SessionContext sessionId={id!} />
                  {status.kind !== "complete" && status.kind !== "error" && (
                    <LiveBadge />
                  )}
                </>
              }
              actions={
                <>
                  <StartNewSessionButton />
                  {meta?.viewer_is_author && (
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setRenameOpen(true)}
                    >
                      Rename
                    </Button>
                  )}
                  {meta?.viewer_is_author && (
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setShareOpen(true)}
                    >
                      Share
                    </Button>
                  )}
                  {meta?.viewer_is_author && isLive && (
                      <RestartSessionButton sessionId={id} />
                    )}
                  {meta?.viewer_is_author && isLive && (
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                        disabled={ending}
                        onClick={() => void endLive(id)}
                      >
                        {ending ? "Ending…" : "End session"}
                      </Button>
                    )}
                  {meta && !meta.viewer_is_author && isLive && (
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={dismissing}
                      onClick={() => void dismissLive(id)}
                      title="Remove from the live list (stays in the sessions view)"
                    >
                      {dismissing ? "Closing…" : "Close"}
                    </Button>
                  )}
                </>
              }
            >
              {status.kind === "reconnecting" && (
                <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-300">
                  Connection dropped — reconnecting…
                </div>
              )}

              {status.kind === "complete" && (
                <div className="mt-3 rounded-lg border border-border bg-muted/40 px-3 py-2 text-sm">
                  This session has ended.{" "}
                  <Link
                    to={`/sessions/${encodeURIComponent(status.sessionId)}`}
                    className="font-medium text-primary hover:underline"
                  >
                    View the full conversation →
                  </Link>
                </div>
              )}
            </SessionDetailHeader>

            {meta?.viewer_is_author && (
              <RenameSessionDialog
                id={id}
                title={title}
                open={renameOpen}
                onOpenChange={setRenameOpen}
              />
            )}

            {meta?.viewer_is_author && (
              <LiveShareDialog
                liveId={id}
                title={title}
                open={shareOpen}
                onOpenChange={setShareOpen}
              />
            )}
        </>

        <div
          className={`grid grid-cols-1 gap-4 [&>*]:min-w-0 ${isLive ? "@7xl:grid-cols-3" : ""}`}
        >
          <div>
            <PullRequestsCard sessionId={id} />
          </div>
          <div>
            <LinearDocsCard sessionId={id} />
          </div>
          {isLive && (
            <PrActivityPanel
              id={id}
              viewerIsAuthor={meta?.viewer_is_author ?? false}
              // Central CLI attachment no longer exists: interactive replies
              // are private-channel operations on the host's own page.
              cliAttached={false}
            />
          )}
        </div>

        {status.kind === "error" ? (
          <ErrorState
            title="Could not load live session"
            message={status.message}
          />
        ) : (
          <div>
            <button
              type="button"
              aria-expanded={transcriptOpen}
              onClick={() => setTranscriptOpen((open) => !open)}
              className="mb-2 flex items-center gap-1.5 text-sm text-muted-foreground transition-colors hover:text-foreground"
            >
              <span
                aria-hidden
                className={`inline-block transition-transform duration-150 ${transcriptOpen ? "rotate-90" : ""}`}
              >
                ▶
              </span>
              {transcriptOpen ? "Hide" : "Show"} full conversation
              {outline.count > 0 && ` (${outline.count})`}
            </button>
            {transcriptOpen &&
              (rendered.length === 0 ? (
                <p className="py-10 text-center text-sm text-muted-foreground">
                  {outline.count > 0
                    ? "Loading conversation…"
                    : "Waiting for the first message…"}
                </p>
              ) : (
                <div className="space-y-4">
                  {rendered.map((m, i) => {
                    const isAssistant = m.role === "assistant";
                    const prev = rendered[i - 1];
                    const next = rendered[i + 1];
                    const runStart = !isAssistant || prev?.role !== "assistant";
                    const runEnd = !isAssistant || next?.role !== "assistant";
                    return (
                      <MessageBubble
                        key={m.uuid}
                        message={m}
                        results={toolResults}
                        runStart={runStart}
                        runEnd={runEnd}
                      />
                    );
                  })}
                </div>
              ))}
          </div>
        )}

        {transcriptOpen && rendered.length > 0 && (
          <ScrollNav
            showUp={!atTop}
            showDown={!atBottom}
            onUp={scrollToTop}
            onDown={scrollToBottom}
          />
        )}

      </div>
    </PrDetectProvider>
  );
}

function BackLink() {
  return (
    <Link
      to="/"
      className="inline-flex items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-foreground"
    >
      ← Back to sessions
    </Link>
  );
}
