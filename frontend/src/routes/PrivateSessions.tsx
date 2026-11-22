import { AttentionDialog } from "@/components/AttentionDialog";
import { SessionLinearProvider } from "@/lib/sessionLinearSource";
import { LinearDocsCard } from "@/components/LinearDocsCard";
import { SessionPrProvider } from "@/lib/sessionPrSource";
import { PullRequestsCard } from "@/components/PullRequestsCard";
import { PrActivityPanel } from "@/components/PrActivityPanel";
import { exactTitleFirst } from "@/lib/sessionSearchRank";
import { RenameSessionDialog } from "@/components/RenameSessionDialog";
import { SessionCollectionToolbar } from "@/components/SessionCollectionToolbar";
import { type SortOrder } from "@/components/SortControl";
import { ReplyComposer } from "@/components/ReplyComposer";
import { useActiveHost } from "@/lib/useActiveHost";
export { useActiveHost } from "@/lib/useActiveHost";
import { AllHostSessionTabs } from "@/components/LiveTabs";
import * as React from "react";
import {
  Link,
  useLocation,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  PrivateSessionCard,
  privateSessionTitle,
} from "@/components/PrivateSessionCard";
import { MessageBubble } from "@/components/MessageBubble";
import { LiveTerminal } from "@/components/LiveTerminal";
import { SessionShareDialog } from "@/components/SessionShareDialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useToast } from "@/components/ui/toast";
import { EmptyState, ErrorState, Spinner } from "@/components/states";
import { FullscreenPending } from "@/components/FullscreenPending";
import { hostControl, HostControlError, unlockAgain } from "@/lib/hostControl";
import {
  listSessions,
  withoutReadiness,
  readHistory,
  refreshHistory,
  type SessionEntry,
  type PrivateMessage,
  type SearchHit,
} from "@/lib/privateChannel";
import { filterClosing, privateScope } from "@/lib/pendingClose";
import { buildToolResultMap, filterRenderedMessages } from "@/lib/toolResults";
import { ALL_AGENTS, type AgentKind, type Message } from "@/lib/types";

/** How often an open private session checks its host for new or changed messages. */
const REFRESH_MS = 5_000;

/**
 * Sessions read directly from your host over the private channel. Nothing here
 * comes from, or is sent to, the central server: if the channel cannot be
 * opened the page says so and shows nothing else.
 */
const NOT_PAIRED = new HostControlError(
  "Pair a host in Settings first: no host is trusted by this browser.",
  "not_paired",
);

/** The query cache for one host's session list; never shared between hosts. */
const privateSessionsKey = (hostId: string | null) =>
  ["private", hostId, "sessions"] as const;

export function useHostSessions(
  hostId: string | null,
  currentId?: string,
  allowed = true,
) {
  const archive = useQuery({
    queryKey: privateSessionsKey(hostId),
    queryFn: () =>
      hostControl.privateRead(hostId!, (c) => listSessions(c), {
        allowUnlock: false,
      }),
    enabled: hostId !== null && allowed,
    retry: false,
    staleTime: 300_000,
  });
  const active = useQuery({
    queryKey: ["private", hostId, "active-sessions"],
    queryFn: async () => {
      const list = await hostControl.privateRead(
        hostId!,
        (c) => listSessions(c, { runningOnly: true }),
        { allowUnlock: false },
      );
      return filterClosing(privateScope(hostId!), list);
    },
    enabled: hostId !== null && allowed,
    retry: false,
    refetchInterval: REFRESH_MS,
  });
  const data = React.useMemo(() => {
    if (!allowed) return undefined;
    const observed = (sessions: SessionEntry[] | undefined) =>
      active.isError && sessions ? withoutReadiness(sessions) : sessions;
    if (!archive.data) {
      if (
        currentId &&
        !active.data?.some((entry) => entry.id === currentId && entry.running)
      )
        return undefined;
      return observed(active.data);
    }
    if (!active.data) return observed(archive.data);
    const live = new Map(active.data.map((entry) => [entry.id, entry]));
    return observed([
      ...archive.data.map((entry) => {
        const current = live.get(entry.id);
        live.delete(entry.id);
        return (
          current ?? (entry.running ? { ...entry, running: false } : entry)
        );
      }),
      ...live.values(),
    ]);
  }, [archive.data, active.data, active.isError, currentId, allowed]);
  return {
    ...archive,
    data,
    isFetched: archive.isFetched || !!data,
    isLoading: !data && (archive.isLoading || active.isLoading),
    isError: !data && archive.isError && active.isError,
    error: data ? null : (active.error ?? archive.error),
    refetch: async () => {
      await Promise.all([archive.refetch(), active.refetch()]);
    },
  };
}

function UnlockPrivate({
  hostId,
  onUnlocked,
}: {
  hostId: string;
  onUnlocked: () => void;
}) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState("");
  return (
    <div className="space-y-2">
      <Button
        disabled={busy}
        onClick={() => {
          setBusy(true);
          setError("");
          void unlockAgain(hostId)
            .then(onUnlocked, (err: Error) => setError(err.message))
            .finally(() => setBusy(false));
        }}
      >
        {busy ? "Unlocking…" : "Unlock with passkey"}
      </Button>
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}

function unavailable(error: unknown): {
  title: string;
  message: string;
  action?: React.ReactNode;
} {
  const action = (
    <Link
      to="/settings"
      className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-accent"
    >
      Set up access
    </Link>
  );
  if (error instanceof HostControlError && error.code === "not_paired") {
    return {
      title: "No host is paired",
      message: "Pair your host in Settings to read its sessions privately.",
      action,
    };
  }
  if (error instanceof HostControlError && error.code === "refused") {
    return {
      title: "Your host refused access",
      message: `${error.message} Review this browser's approval and permissions in Settings → Host access, or unlock it again with your passkey. Nothing was read from the server instead.`,
      action,
    };
  }
  return {
    title: "Your host is not reachable privately",
    message: `${(error as Error)?.message ?? "The private connection failed."} Nothing was read from the server instead.`,
  };
}

export function PrivateSessionList({
  embedded = false,
}: {
  embedded?: boolean;
}) {
  const hostId = useActiveHost();
  const [recentLimit, setRecentLimit] = React.useState(24);
  const [emptyLimit, setEmptyLimit] = React.useState(24);
  const [emptyOpen, setEmptyOpen] = React.useState(false);
  const [search, setSearch] = React.useState("");
  const [project, setProject] = React.useState("");
  const [sort, setSort] = React.useState<SortOrder>("activity_desc");
  React.useEffect(() => {
    setRecentLimit(24);
    setEmptyLimit(24);
    setEmptyOpen(false);
    setSearch("");
    setProject("");
    setSort("activity_desc");
  }, [hostId]);
  const { data, isLoading, error, refetch } = useHostSessions(hostId);
  if (!hostId)
    return embedded ? null : <ErrorState {...unavailable(NOT_PAIRED)} />;
  if (isLoading) return <Spinner label="Connecting to your host privately…" />;
  if (error) {
    const failure = unavailable(error);
    return (
      <div className="space-y-3">
        <ErrorState
          {...failure}
          onRetry={failure.action ? undefined : () => void refetch()}
        />
        {error instanceof HostControlError && error.code === "locked" && (
          <UnlockPrivate hostId={hostId} onUnlocked={() => void refetch()} />
        )}
      </div>
    );
  }
  if (!data?.length) {
    return (
      <EmptyState
        title="No sessions you can read"
        message="Your host lists only the sessions this browser is allowed to read."
      />
    );
  }
  const activity = (value: string | null) =>
    value ? Date.parse(value) || 0 : 0;
  const terms = search.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
  const matches = data.filter((session) => {
    const text =
      `${privateSessionTitle(session)} ${session.cwd} ${session.agent}`.toLocaleLowerCase();
    return (
      (!project || session.cwd === project) &&
      terms.every((term) => text.includes(term))
    );
  });
  const sorted = exactTitleFirst(
    matches.sort(
      (a, b) =>
        Number(b.running) - Number(a.running) ||
        (sort === "activity_asc" ? 1 : -1) *
          (activity(a.last_activity) - activity(b.last_activity)) ||
        a.id.localeCompare(b.id),
    ),
    search,
    privateSessionTitle,
  );
  const running = sorted.filter((s) => s.running);
  const recent = sorted.filter(
    (s) =>
      !s.running &&
      (s.messages > 0 || s.reopenable === true || terms.length > 0),
  );
  const visible = exactTitleFirst(
    [...running, ...recent.slice(0, recentLimit)],
    search,
    privateSessionTitle,
  );
  const empty = sorted.filter(
    (s) =>
      !s.running &&
      s.messages === 0 &&
      s.reopenable !== true &&
      terms.length === 0,
  );
  return (
    <div className="space-y-4" data-testid="private-sessions">
      {embedded ? (
        <h2 className="text-lg font-semibold">On your laptop</h2>
      ) : (
        <h1 className="text-lg font-semibold">Private sessions</h1>
      )}
      <SessionCollectionToolbar
        search={search}
        onSearch={(value) => {
          setSearch(value);
          setRecentLimit(24);
          setEmptyLimit(24);
        }}
        project={project}
        onProject={setProject}
        projects={Array.from(new Set(data.map((entry) => entry.cwd)))
          .sort()
          .map((cwd) => ({
            project_path: cwd,
            session_count: data.filter((entry) => entry.cwd === cwd).length,
          }))}
        sort={sort}
        onSort={setSort}
        sortOptions={["activity_desc", "activity_asc"]}
        searchLabel="Search private sessions"
        searchPlaceholder="Search names, folders, or agents…"
      />
      {matches.length === 0 && (
        <p className="text-sm text-muted-foreground">
          No matching private sessions.
        </p>
      )}
      <div className="grid items-start gap-3 sm:grid-cols-2">
        {visible.map((session) => (
          <PrivateSessionCard key={session.id} session={session} />
        ))}
      </div>
      {recent.length > recentLimit && (
        <Button
          variant="outline"
          onClick={() => setRecentLimit((limit) => limit + 24)}
        >
          Show more stopped sessions ({recent.length - recentLimit} remaining)
        </Button>
      )}
      {empty.length > 0 && (
        <details
          open={emptyOpen}
          onToggle={(event) => setEmptyOpen(event.currentTarget.open)}
          className="rounded-xl border border-border p-4"
        >
          <summary className="cursor-pointer text-sm text-muted-foreground">
            Stopped sessions without messages ({empty.length})
          </summary>
          <div className="mt-4 grid items-start gap-3 sm:grid-cols-2">
            {emptyOpen &&
              empty
                .slice(0, emptyLimit)
                .map((session) => (
                  <PrivateSessionCard key={session.id} session={session} />
                ))}
            {emptyOpen && empty.length > emptyLimit && (
              <Button
                variant="outline"
                onClick={() => setEmptyLimit((limit) => limit + 24)}
              >
                Show more empty sessions ({empty.length - emptyLimit} remaining)
              </Button>
            )}
          </div>
        </details>
      )}
    </div>
  );
}

export function PrivateSessionView() {
  const { id = "" } = useParams<{ id: string }>();
  const selectedHost = useActiveHost();
  const [params, setParams] = useSearchParams();
  const location = useLocation();
  const hostId = params.get("host") ?? selectedHost;
  React.useEffect(() => {
    if (!params.has("host") && selectedHost) {
      const next = new URLSearchParams(params);
      next.set("host", selectedHost);
      // Carry the navigation state over: "New session" uses it to ask the
      // terminal to fit its grid to this screen once the session has loaded.
      setParams(next, { replace: true, state: location.state });
    }
  }, [params, selectedHost, setParams, location.state]);
  // Remount all session-owned state before rendering another route or host.
  return (
    <SessionPrProvider hostId={hostId ?? undefined}>
      <SessionLinearProvider hostId={hostId ?? undefined}>
        <PrivateSessionContent
          key={`${hostId}:${id}`}
          hostId={hostId}
          id={id}
        />
      </SessionLinearProvider>
    </SessionPrProvider>
  );
}

function PrivateSessionContent({
  hostId,
  id,
}: {
  hostId: string | null;
  id: string;
}) {
  const { toast } = useToast();
  const queryClient = useQueryClient();
  const [searchParams] = useSearchParams();
  // Everything on this page belongs to ONE host: reads, search, replies, key
  // access and publication all name it, and a host switch starts over.
  const [renaming, setRenaming] = React.useState(false);
  // The host's list says whether the session runs; opened directly (a link
  // or a reload) the page asks for it rather than assuming it does not.
  const {
    data: sessions,
    isFetched: sessionsFetched,
    error: sessionsError,
    refetch: refetchSessions,
  } = useHostSessions(hostId, id);
  const entry = sessions?.find((s) => s.id === id);
  const title = entry ? privateSessionTitle(entry) : "This session";
  const readOnly = entry?.reopenable === false;
  const continuation = entry?.resume_mode === "continuation";
  const [reopening, setReopening] = React.useState(false);
  const context = React.useRef("");
  context.current = `${hostId}:${id}`;
  const agent = ALL_AGENTS.find((a: AgentKind) => a === entry?.agent);
  // Held with the host it was opened for, so a host switch cannot render it
  // against the new host before the reset below runs.
  // The private terminal (and its compact row with tabs + Actions + Display)
  // is the primary pane whenever the session is running — a dedicated
  // Show/Hide toggle would just hide the controls it hosts.
  const terminal = hostId !== null && entry?.running === true;
  const [transcriptOpen, setTranscriptOpen] = React.useState(false);
  const showHistory = !entry?.running || transcriptOpen;
  // Fullscreen first-paint contract: a `?fullscreen=1` deep link paints the
  // fullscreen terminal shell from the very first render. While the shell is
  // up (host/entry still loading, or the session has ended) nothing else
  // renders — so don't start reading history either.
  const fullscreen = searchParams.get("fullscreen") === "1";
  const fullscreenPending = fullscreen && !terminal;
  const readEnabled = sessionsFetched && showHistory && !fullscreenPending;
  const [messages, setMessages] = React.useState<PrivateMessage[] | null>(null);
  const [error, setError] = React.useState<unknown>(null);
  const [query, setQuery] = React.useState("");
  const [hits, setHits] = React.useState<SearchHit[] | null>(null);
  const [attention, setAttention] = React.useState(false);
  const [sharing, setSharing] = React.useState(false);
  const [loadingHistory, setLoadingHistory] = React.useState(true);
  const [readAttempt, setReadAttempt] = React.useState(0);

  React.useEffect(() => {
    if (!readEnabled) return;
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let progressTimer: ReturnType<typeof setTimeout> | undefined;
    let known: PrivateMessage[] | null = null;
    const loaded: PrivateMessage[] = [];
    let firstPage = true;
    setError(null);
    setLoadingHistory(true);
    if (!hostId) {
      setError(NOT_PAIRED);
      setLoadingHistory(false);
      return;
    }
    const flush = () => {
      progressTimer = undefined;
      if (!abort.signal.aborted) setMessages(loaded.slice());
    };
    const onPage = (page: PrivateMessage[]) => {
      if (abort.signal.aborted) return;
      loaded.push(...page);
      if (firstPage) {
        firstPage = false;
        flush();
      } else if (!progressTimer) {
        // Coalesce quick pages so a long transcript is not rebuilt on every read.
        progressTimer = setTimeout(flush, 150);
      }
    };
    const tick = async () => {
      try {
        const next = known
          ? (
              await hostControl.privateRead(
                hostId,
                (c) => refreshHistory(c, id, known!, { signal: abort.signal }),
                { allowUnlock: false },
              )
            ).messages
          : await hostControl.privateRead(
              hostId,
              (c) => readHistory(c, id, { signal: abort.signal, onPage }),
              { allowUnlock: false },
            );
        if (abort.signal.aborted) return;
        if (progressTimer) clearTimeout(progressTimer);
        progressTimer = undefined;
        // Reuse the previous array when a refresh found no changed messages.
        if (
          !known ||
          known.length !== next.length ||
          next.some((m, i) => m !== known![i])
        )
          setMessages(next);
        known = next;
        setError(null);
        setLoadingHistory(false);
        timer = setTimeout(() => void tick(), REFRESH_MS);
      } catch (e) {
        if (!abort.signal.aborted) {
          if (progressTimer) {
            clearTimeout(progressTimer);
            flush();
          }
          setError(e);
          setLoadingHistory(false);
        }
      }
    };
    void tick();
    return () => {
      abort.abort();
      if (timer) clearTimeout(timer);
      if (progressTimer) clearTimeout(progressTimer);
    };
  }, [hostId, id, readAttempt, readEnabled]);

  React.useEffect(() => {
    if (!reopening) return;
    if (entry?.running) {
      setReopening(false);
      toast("Session is running on your host.");
      return;
    }
    const timer = setTimeout(() => {
      setReopening(false);
      toast(
        "Your host has not reported this session running yet. Check the host and try again.",
        "error",
      );
    }, 30_000);
    return () => clearTimeout(timer);
  }, [reopening, entry?.running, toast]);

  const reopen = async () => {
    if (!hostId || !entry || entry.running || readOnly || reopening) return;
    const startedFor = context.current;
    setReopening(true);
    try {
      await hostControl.reopenSession(id);
      await queryClient.invalidateQueries({
        queryKey: privateSessionsKey(hostId),
      });
    } catch (e) {
      if (context.current !== startedFor) return;
      setReopening(false);
      toast((e as Error).message, "error");
    }
  };

  const search = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!query.trim() || !hostId) return setHits(null);
    try {
      const result = await hostControl.privateRead(hostId, (c) =>
        c.historySearch(id, query.trim()),
      );
      setHits(result.body?.kind === "search" ? result.body.hits : []);
    } catch (e) {
      toast((e as Error).message, "error");
    }
  };

  if (fullscreenPending) {
    const locked =
      sessionsError instanceof HostControlError &&
      sessionsError.code === "locked";
    const stopped = entry && entry.running === false;
    return (
      <FullscreenPending
        title={entry ? title : undefined}
        status={
          locked
            ? {
                kind: "error",
                label: "Unlock this device to open your private session.",
              }
            : sessionsError
              ? { kind: "error", label: (sessionsError as Error).message }
              : stopped
                ? { kind: "stopped" }
                : { kind: "connecting", label: "Connecting to your host…" }
        }
        action={
          locked && hostId ? (
            <UnlockPrivate
              hostId={hostId}
              onUnlocked={() => void refetchSessions()}
            />
          ) : undefined
        }
      />
    );
  }

  if (
    !entry &&
    sessionsError instanceof HostControlError &&
    sessionsError.code === "locked" &&
    hostId
  ) {
    return (
      <div className="space-y-3">
        <p>Unlock this device to open your private session.</p>
        <UnlockPrivate
          hostId={hostId}
          onUnlocked={() => void refetchSessions()}
        />
      </div>
    );
  }

  const failure = error == null ? null : unavailable(error);

  return (
    <div className="space-y-4" data-testid="private-session">
      <Link
        to="/"
        className="inline-flex items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-foreground"
      >
        ← Sessions
      </Link>
      {/* Reopen stays at the page level because the live terminal — and its
          Actions menu — do not render until the session is running. Rename,
          Share and Ask for attention live inside the
          terminal's Actions menu once the terminal is up. */}
      {!terminal && (
        <div className="flex flex-wrap items-center gap-2">
          <h1
            className="mr-auto truncate text-base font-semibold"
            title={entry?.cwd}
          >
            {title}
          </h1>
          {entry && !entry.running && (
            <Button
              size="sm"
              variant="outline"
              disabled={!hostId || readOnly || reopening}
              onClick={() => void reopen()}
            >
              {reopening
                ? "Reopening…"
                : continuation
                  ? "Continue locally"
                  : "Reopen"}
            </Button>
          )}
          {hostId && (
            <Button
              size="sm"
              variant="outline"
              onClick={() => setRenaming(true)}
            >
              Rename
            </Button>
          )}
          <Button
            size="sm"
            variant="outline"
            disabled={!hostId}
            onClick={() => setSharing(true)}
            title="Choose who can read this session and who can type into it"
          >
            Share…
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setAttention(true)}
          >
            Ask for attention
          </Button>
        </div>
      )}
      {hostId && (
        <RenameSessionDialog
          id={id}
          title={title}
          open={renaming}
          onOpenChange={setRenaming}
          onRename={async (next) => {
            const effective = await hostControl.renameSession(id, next, hostId);
            await queryClient.invalidateQueries({
              queryKey: ["private", hostId],
            });
            return effective;
          }}
        />
      )}
      {/* When the terminal is up, its compact row carries the tabs on the
          left. Fallback: surface the tab bar here when the terminal is not
          rendered (no live session, or the session has ended). */}
      {hostId && !terminal && !fullscreen && (
        <AllHostSessionTabs
          sessions={sessions ?? []}
          hostId={hostId}
          currentId={id}
        />
      )}
      {continuation && !readOnly && !entry?.running && (
        <p className="text-sm text-muted-foreground">
          Continue in a new agent session with the imported history as context.
        </p>
      )}
      {readOnly && (
        <p className="text-sm text-muted-foreground">
          Read-only history: native agent data is unavailable on this host, so
          this session cannot be reopened.
        </p>
      )}
      {reopening && (
        <p role="status" className="text-sm text-muted-foreground">
          Waiting for your host to report the session running…
        </p>
      )}
      {hostId && (
        <SessionShareDialog
          hostId={hostId}
          sessionId={id}
          title={title}
          open={sharing}
          onOpenChange={setSharing}
        />
      )}
      {failure && (
        <div role="alert" className="space-y-2 text-sm text-destructive">
          <p>
            {failure.title}: {failure.message}
          </p>
          {failure.action}
          {messages && (
            <p>
              Showing the complete messages already read. History may be
              incomplete.
            </p>
          )}
          {!failure.action && (
            <Button
              variant="outline"
              size="sm"
              onClick={() => setReadAttempt((n) => n + 1)}
            >
              Retry history
            </Button>
          )}
        </div>
      )}
      {/* Live terminal renders first below the header/tabs so the running agent
          is the primary visible content, mirroring the shared live view. PR
          and Linear metadata and the activity rail follow underneath. */}
      {terminal && (
        // The live terminal's own renderer and controls; only its source
        // changes: the screen, input, files and images use this host's
        // private channel. The host decides whether this key may type.
        <LiveTerminal
          key={`${hostId}:${id}`}
          id={id}
          agent={agent}
          privateHostId={hostId}
          privateSessions={sessions ?? []}
          viewerCanWrite={!readOnly}
        />
      )}
      {hostId && (
        <div className="grid grid-cols-1 gap-4 [&>*]:min-w-0 @7xl:grid-cols-2">
          <div className={terminal ? "xl:hidden" : ""}>
            <PullRequestsCard sessionId={id} />
          </div>
          <div className={terminal ? "xl:hidden" : ""}>
            <LinearDocsCard sessionId={id} />
          </div>
          <PrActivityPanel
            id={id}
            canManage={!readOnly}
            interactive={entry?.running === true}
            cliAttached={entry?.running === true}
          />
        </div>
      )}

      {attention && hostId && (
        <AttentionDialog
          sessionId={id}
          hostId={hostId}
          title={entry?.title || id}
          onOpenChange={setAttention}
        />
      )}
      {entry?.running && (
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
          {entry.messages > 0 && ` (${entry.messages})`}
        </button>
      )}
      <div hidden={!showHistory}>
        <form
          onSubmit={(e) => void search(e)}
          className="flex gap-2"
          role="search"
        >
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search this session on your host"
            aria-label="Search this session"
          />
          <Button type="submit" size="sm" variant="outline">
            Search
          </Button>
        </form>
        {hits && (
          <ul className="space-y-1 text-sm" aria-label="Search results">
            {hits.length === 0 && (
              <li className="text-muted-foreground">No matches.</li>
            )}
            {hits.map((h) => (
              <li key={`${h.seq}:${h.uuid}`}>
                <a href={`#m-${h.uuid}`} className="hover:underline">
                  {h.snippet}
                </a>
              </li>
            ))}
          </ul>
        )}
        {loadingHistory &&
          (messages === null ? (
            <Spinner label="Reading the first history page privately…" />
          ) : (
            <p role="status" className="text-sm text-muted-foreground">
              Loading more history… {messages.length} messages loaded.
            </p>
          ))}
        {messages && <PrivateTranscript messages={messages} />}
        <ReplyComposer
          id={id}
          hostId={hostId ?? undefined}
          mode="interactive"
          viewerCanWrite={!readOnly}
        />
      </div>
    </div>
  );
}

/** Typing, session polling and terminal controls do not rebuild the transcript. */
const PrivateTranscript = React.memo(function PrivateTranscript({
  messages,
}: {
  messages: PrivateMessage[];
}) {
  const rendered = filterRenderedMessages(
    messages.map((m) => m.message as unknown as Message),
  );
  const results = buildToolResultMap(rendered);
  return (
    <div className="space-y-4">
      {rendered.map((m, i) => {
        const isAssistant = m.role === "assistant";
        return (
          <div key={m.uuid} id={`m-${m.uuid}`}>
            <MessageBubble
              message={m}
              results={results}
              runStart={!isAssistant || rendered[i - 1]?.role !== "assistant"}
              runEnd={!isAssistant || rendered[i + 1]?.role !== "assistant"}
            />
          </div>
        );
      })}
    </div>
  );
});
