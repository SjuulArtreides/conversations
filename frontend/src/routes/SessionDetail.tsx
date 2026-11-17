import { SessionContext } from "@/components/SessionContext";
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { useAuth, useRedirectOnUnauthorized } from "@/lib/useAuth";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { VisibilityBadge } from "@/components/VisibilityBadge";
import { MessageBubble } from "@/components/MessageBubble";
import { buildToolResultMap, filterRenderedMessages } from "@/lib/toolResults";
import { ShareDialog } from "@/components/ShareDialog";
import { RepoDialog } from "@/components/RepoDialog";
import { DeleteSessionDialog } from "@/components/DeleteSessionDialog";
import { RenameSessionDialog } from "@/components/RenameSessionDialog";
import {
  ReopenSessionButton,
  StartNewSessionButton,
} from "@/components/HostActions";
import { LiveTabs } from "@/components/LiveTabs";
import { SessionActions } from "@/components/SessionActions";
import { useLiveSessions } from "@/lib/useLiveSessions";
import { SessionSummaryCard } from "@/components/SessionSummaryCard";
import { ChatPanel } from "@/components/ChatPanel";
import { PullRequestsCard } from "@/components/PullRequestsCard";
import { LinearDocsCard } from "@/components/LinearDocsCard";
import { PrActivityPanel } from "@/components/PrActivityPanel";
import { PrDetectProvider, usePrDetectValue } from "@/components/PrDetect";
import { deriveRepoContextFromTexts } from "@/lib/prRefs";
import { ProjectAssign } from "@/components/ProjectAssign";
import { ErrorState, Spinner } from "@/components/states";
import { fullTimestamp, humanizeTime, shortProject } from "@/lib/utils";
import type { RepoLink, SessionHandoff } from "@/lib/types";

/** localStorage key used by ChatPanel to track its docked open/closed state. */
const CHAT_PANEL_OPEN_KEY = "conversations.chatPanelOpen";

export function SessionDetail() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const { user, authEnabled, isLoading: authLoading } = useAuth();
  const [shareOpen, setShareOpen] = React.useState(false);
  const [reposOpen, setReposOpen] = React.useState(false);
  const [deleteOpen, setDeleteOpen] = React.useState(false);
  const [renameOpen, setRenameOpen] = React.useState(false);
  // Offering a key (EX-03): the invite link the colleague pastes is a
  // /host-link URL whose fragment carries the whole pairing + enrollment
  // ceremony; submitting it routes there.
  const [offerStarted, setOfferStarted] = React.useState(false);
  const [inviteLink, setInviteLink] = React.useState("");

  // Mirror the docked panel's open state so we can reserve space in the
  // main column (right padding) and avoid content sliding behind the rail.
  const [chatOpen, setChatOpen] = React.useState<boolean>(() => {
    try {
      return localStorage.getItem(CHAT_PANEL_OPEN_KEY) === "true";
    } catch {
      return false;
    }
  });

  // Keep chatOpen in sync whenever the panel is toggled (it writes to
  // localStorage, so we poll on storage events and on focus).
  React.useEffect(() => {
    function sync() {
      try {
        setChatOpen(localStorage.getItem(CHAT_PANEL_OPEN_KEY) === "true");
      } catch {
        // ignore
      }
    }
    window.addEventListener("storage", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("storage", sync);
      window.removeEventListener("focus", sync);
    };
  }, []);

  const { data, isLoading, isError, error, refetch } = useQuery({
    queryKey: ["session", id],
    queryFn: () => api.getSession(id),
    enabled: !!id,
    // A pending handoff settles when the host reports its startup result.
    refetchInterval: (query) =>
      handoffPending(query.state.data?.handoff) ? 3000 : false,
  });

  useRedirectOnUnauthorized(error);

  const isOwner = !!user && data?.owner_email === user.email;
  const canManage = !authLoading && (!authEnabled || isOwner);
  const ownerless = !authLoading && authEnabled && data?.owner_email == null;

  // Surface the viewer's active live sessions on top so you can jump to one
  // even while reading a stored session. The shared order also controls shortcuts.
  // LiveTabs hides itself when nothing other than this id is live.
  const { list: liveList } = useLiveSessions();
  const orderedLiveSessions = liveList;

  // In-message PR detection (computed before the early returns so hook order is
  // stable). Repo context comes from full PR URLs across the transcript; the
  // "Add to session details" affordance is owner-only, mirroring the delete gate.
  const repoCtx = React.useMemo(
    () => deriveRepoContextFromTexts((data?.messages ?? []).map((m) => m.text)),
    [data],
  );
  const prDetect = usePrDetectValue({
    sessionId: id,
    repoCtx,
    canAdd:
      !user || data?.owner_email == null || data?.owner_email === user.email,
    enabled: !!data,
  });

  // Fold each tool_result into its originating tool_use box: build a
  // session-wide id→result lookup, then drop the now-redundant pure-result
  // messages. Pure functions (not hooks), so it's fine after the early returns.
  const toolResults = data ? buildToolResultMap(data.messages) : undefined;
  const renderedMessages = data ? filterRenderedMessages(data.messages) : [];

  if (isLoading) return <Spinner label="Loading session…" />;
  if (isError || !data)
    return (
      <div className="space-y-4">
        <BackLink />
        <ErrorState
          title="Could not load session"
          message={(error as Error)?.message}
          onRetry={() => refetch()}
        />
      </div>
    );

  return (
    // When the chat rail is open we add right padding (lg+ only) so the 360px
    // panel doesn't overlap the main content on wide screens. The panel is
    // 360px wide, so pr-[376px] (360 + 16px gap) keeps things clear. Below lg
    // the panel overlays the content — full-width on phones, a rail on tablets
    // — which is the intended mobile behaviour; reserving 376px there would
    // crush the main column.
    <PrDetectProvider value={prDetect}>
      <div
        className={`space-y-5 transition-[padding] duration-200${chatOpen ? " lg:pr-[376px]" : ""}`}
      >
        <BackLink />

        <LiveTabs sessions={orderedLiveSessions} currentId={id} />

        {data.viewer_write_pending_key && (
          <div
            role="note"
            aria-label="Write waiting for key trust"
            className="rounded-xl border border-amber-500/40 bg-amber-500/10 p-4 text-sm"
          >
            <p className="font-medium">
              You can read this, but you can&rsquo;t type into it yet.
            </p>
            <p className="text-muted-foreground">
              {data.owner_email ?? "The session’s owner"} named you as someone
              who should be able to work in this session. Your key isn’t trusted
              on their host, so typing stays blocked until they approve it.
            </p>
            {!offerStarted ? (
              <Button
                size="sm"
                variant="outline"
                onClick={() => setOfferStarted(true)}
              >
                Offer your key
              </Button>
            ) : (
              <form
                className="mt-2 flex flex-wrap gap-2"
                onSubmit={(event) => {
                  event.preventDefault();
                  // The invite link is a /host-link URL whose fragment carries
                  // the whole ceremony; it stays in this browser.
                  const hash = inviteLink.slice(inviteLink.indexOf("#") + 1);
                  if (hash && hash !== inviteLink) {
                    navigate(`/host-link#${hash}`);
                  }
                }}
              >
                <Input
                  value={inviteLink}
                  onChange={(event) => setInviteLink(event.target.value)}
                  placeholder="Paste the invite link they sent you"
                  aria-label="Invite link"
                  className="min-w-60 flex-1"
                />
                <Button
                  type="submit"
                  size="sm"
                  disabled={!inviteLink.includes("#")}
                >
                  Offer your key
                </Button>
              </form>
            )}
            <p className="mt-1 text-xs text-muted-foreground">
              Ask {data.owner_email ?? "the session's owner"} for their key
              invite link, then offer your key here.
            </p>
          </div>
        )}

        <div className="rounded-xl border border-border bg-card p-5">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <h1 className="text-xl font-semibold leading-snug tracking-tight">
                {data.title}
              </h1>
              <SessionContext sessionId={id!} />
              <VisibilityBadge visibility={data.visibility} />
            </div>
            <SessionActions>
              {data.cwd && isOwner && (
                <ReopenSessionButton sessionId={data.id} />
              )}
              {/* Start a brand-new live session from here too (independent of
                  this stored one). Self-gates on host availability — enabled
                  when a CLI host is attached, disabled with a tooltip when not,
                  mirroring the "Re-open as live" button beside it. */}
              <StartNewSessionButton />
              {(canManage || ownerless) && (
                <>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setShareOpen(true)}
                    disabled={!canManage}
                    aria-describedby={
                      !canManage ? "session-ownership-reason" : undefined
                    }
                  >
                    Share
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setRenameOpen(true)}
                    disabled={!canManage}
                    aria-describedby={
                      !canManage ? "session-ownership-reason" : undefined
                    }
                  >
                    Rename
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                    onClick={() => setDeleteOpen(true)}
                    disabled={!canManage}
                    aria-describedby={
                      !canManage ? "session-ownership-reason" : undefined
                    }
                  >
                    Delete
                  </Button>
                </>
              )}
            </SessionActions>
          </div>

          {ownerless && (
            <p
              id="session-ownership-reason"
              className="mt-3 text-xs text-muted-foreground"
            >
              This session has no owner, so nobody can change it.
            </p>
          )}

          <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            {data.project_path && (
              <span
                className="font-mono text-foreground/80"
                title={data.project_path}
              >
                {shortProject(data.project_path)}
              </span>
            )}
            {data.git_branch && data.project_is_container !== true && (
              <Badge variant="branch" className="py-0">
                ⎇ {data.git_branch}
              </Badge>
            )}
            {data.claude_version && <span>· Claude {data.claude_version}</span>}
            <span>· {data.message_count.toLocaleString()} messages</span>
            <span title={fullTimestamp(data.uploaded_at)}>
              · uploaded by {data.uploaded_by || "anonymous"}{" "}
              {humanizeTime(data.uploaded_at)}
            </span>
          </div>

          {data.handoff && <HandoffStatus handoff={data.handoff} />}

          {data.skipped_lines > 0 && (
            <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-300">
              ⚠ {data.skipped_lines.toLocaleString()} line
              {data.skipped_lines === 1 ? "" : "s"} could not be parsed and were
              skipped.
            </div>
          )}
        </div>

        <ProjectAssign
          key={data.id}
          sessionId={data.id}
          projectId={data.project_id}
        />

        <SessionSummaryCard session={data} />

        <ChatPanel
          parent={{ kind: "session", id: data.id }}
          docked
          onDockedToggle={setChatOpen}
        />

        {/* Pull Requests section — mirrors the Repositories card layout. */}
        <PullRequestsCard sessionId={data.id} />

        {/* Linear references auto-detected in the transcript, below the PRs. */}
        <LinearDocsCard sessionId={data.id} />

        {/* Cached PR review/issue comments (pushed by the CLI). Read-only here —
          the session is not live, so there's no CLI to move comments into or
          reply through; the panel hides itself when there are none. */}
        <PrActivityPanel
          id={data.id}
          viewerIsAuthor={false}
          interactive={false}
        />

        <RepositoriesCard
          repos={data.repos}
          onEdit={canManage ? () => setReposOpen(true) : undefined}
        />

        <div className="space-y-4">
          {renderedMessages.map((m, i) => {
            // A "run" is a contiguous block of assistant messages (one turn split
            // across thinking / tool_use / text segments). The label+timestamp
            // header shows once at the start of the run and the usage footer once
            // at the end; the middle bubbles are stripped of that chrome. Non-
            // assistant messages are always their own single-bubble run.
            const isAssistant = m.role === "assistant";
            const prev = renderedMessages[i - 1];
            const next = renderedMessages[i + 1];
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

        {canManage && (
          <>
            <ShareDialog
              session={data}
              open={shareOpen}
              onOpenChange={setShareOpen}
            />

            <RepoDialog
              session={data}
              open={reposOpen}
              onOpenChange={setReposOpen}
            />

            <RenameSessionDialog
              id={data.id}
              title={data.title}
              open={renameOpen}
              onOpenChange={setRenameOpen}
            />

            <DeleteSessionDialog
              id={data.id}
              title={data.title}
              open={deleteOpen}
              onOpenChange={setDeleteOpen}
            />
          </>
        )}

        <BackToTop />
      </div>
    </PrDetectProvider>
  );
}

/**
 * Card listing a session's linked repositories. Repos flagged as the viewer's
 * monorepo "container" (`is_container === true`) are hidden from the main list
 * by default — a subtle "N container repo hidden" line with a Show toggle
 * reveals them, rendered de-emphasized with a "container" badge. The Edit
 * dialog still operates on the FULL set (containers included) via `onEdit`.
 */
function RepositoriesCard({
  repos,
  onEdit,
}: {
  repos: RepoLink[];
  onEdit?: () => void;
}) {
  const [showContainers, setShowContainers] = React.useState(false);

  const work = repos.filter((r) => r.is_container !== true);
  const containers = repos.filter((r) => r.is_container === true);

  return (
    <div className="rounded-xl border border-border bg-card p-5">
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-sm font-semibold text-foreground">Repositories</h2>
        {onEdit && (
          <Button size="sm" variant="outline" onClick={onEdit}>
            Edit
          </Button>
        )}
      </div>

      {repos.length === 0 ? (
        <p className="mt-3 text-xs text-muted-foreground">
          No repositories linked.
          {onEdit && " Upload from the CLI to auto-detect, or add them here."}
        </p>
      ) : (
        <>
          {work.length === 0 && !showContainers ? (
            <p className="mt-3 text-xs text-muted-foreground">
              No work repositories — only container repositories are linked.
            </p>
          ) : (
            <ul className="mt-3 space-y-3">
              {work.map((r) => (
                <RepoRow key={`${r.repo}@${r.branch}`} repo={r} />
              ))}
            </ul>
          )}

          {containers.length > 0 && (
            <div className="mt-3">
              <button
                type="button"
                onClick={() => setShowContainers((v) => !v)}
                className="text-xs text-muted-foreground underline-offset-2 transition-colors hover:text-foreground hover:underline"
                data-testid="container-toggle"
              >
                {showContainers
                  ? `Hide ${containers.length} container repo${containers.length === 1 ? "" : "s"}`
                  : `${containers.length} container repo${containers.length === 1 ? "" : "s"} hidden · Show`}
              </button>

              {showContainers && (
                <ul className="mt-3 space-y-3">
                  {containers.map((r) => (
                    <RepoRow
                      key={`${r.repo}@${r.branch}`}
                      repo={r}
                      isContainer
                    />
                  ))}
                </ul>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
}

/** One repository row. Container rows are de-emphasized and carry a badge. */
function RepoRow({
  repo: r,
  isContainer = false,
}: {
  repo: RepoLink;
  isContainer?: boolean;
}) {
  return (
    <li
      className={`rounded-lg border border-border/70 p-3${isContainer ? " opacity-60" : ""}`}
      data-testid={isContainer ? "container-repo-row" : "repo-row"}
    >
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="font-medium text-foreground">{r.repo}</span>
        <Badge variant="branch" className="py-0">
          ⎇ {r.branch}
        </Badge>
        {isContainer && (
          <Badge
            className="border-transparent bg-muted py-0 text-[0.65rem] font-normal text-muted-foreground"
            data-testid="container-badge"
          >
            container
          </Badge>
        )}
        {r.pr_url && (
          <a
            href={r.pr_url}
            target="_blank"
            rel="noreferrer"
            className="text-primary hover:underline"
            onClick={(e) => e.stopPropagation()}
          >
            PR ↗
          </a>
        )}
      </div>
      {r.remote_url && (
        <div
          className="mt-1 truncate font-mono text-xs text-muted-foreground"
          title={r.remote_url}
        >
          {r.remote_url}
        </div>
      )}
      {r.paths.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-xs text-muted-foreground">
          {r.paths.map((p) => (
            <li key={p.user_email} className="flex gap-2">
              <span className="text-foreground/70">{p.user_email}</span>
              <span className="truncate font-mono" title={p.local_path}>
                {p.local_path}
              </span>
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

/**
 * A lightweight floating "scroll to top" button for long transcripts. Hidden
 * until the page has scrolled past a threshold; clicking smooth-scrolls to top.
 */
function BackToTop() {
  const [visible, setVisible] = React.useState(false);

  React.useEffect(() => {
    const onScroll = () => setVisible(window.scrollY > 600);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  if (!visible) return null;

  return (
    <button
      type="button"
      aria-label="Back to top"
      title="Back to top"
      onClick={() => window.scrollTo({ top: 0, behavior: "smooth" })}
      className="fixed bottom-6 right-6 z-40 flex h-10 w-10 items-center justify-center rounded-full border border-border bg-card text-muted-foreground shadow-md transition-colors hover:text-foreground"
    >
      <svg
        className="h-4 w-4"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden
      >
        <path d="m18 15-6-6-6 6" />
      </svg>
    </button>
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

function handoffPending(handoff: SessionHandoff | null | undefined) {
  return handoff?.status === "requested" || handoff?.status === "starting";
}

/** Why a handoff session has no transcript yet, and how to recover. */
function HandoffStatus({ handoff }: { handoff: SessionHandoff }) {
  const source = (
    <Link
      to={`/sessions/${encodeURIComponent(handoff.source_session_id)}`}
      className="underline underline-offset-2"
    >
      source session
    </Link>
  );
  if (handoffPending(handoff))
    return (
      <div
        role="status"
        className="mt-3 rounded-lg border border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground"
      >
        Waiting for your host to start this handoff. The agent appears here once
        its terminal connects. Requested from the {source}.
      </div>
    );
  if (handoff.status !== "failed") return null;
  return (
    <div
      role="alert"
      className="mt-3 space-y-1 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
    >
      <p className="font-medium">Host could not confirm this handoff started</p>
      <p className="font-mono break-words">
        {handoff.error || "The host reported a startup failure."}
      </p>
      <p className="text-foreground/80">
        The agent may or may not have started. Before retrying, check your
        terminal app for a workspace created for this handoff. Once you are sure
        no copy is running, ask the {source} to retry the handoff.
      </p>
    </div>
  );
}
