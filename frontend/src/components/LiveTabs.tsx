import { AgentReadinessIndicator } from "./AgentReadinessIndicator";
import { PrivateSessionActionsMenu } from "./PrivateSessionActionsMenu";
import { hostControl } from "@/lib/hostControl";
import { useToast } from "./ui/toast";
import { useMutation, useQueryClient, useQueries } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { clearClosing, markClosing, privateScope } from "@/lib/pendingClose";
import { LIVE_SESSIONS_QUERY_KEY } from "@/lib/useLiveSessions";
import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { cn } from "@/lib/utils";
import {
  useDismissLiveSession,
  useEndLiveSession,
} from "@/components/HostActions";
import { SessionActionsMenu } from "@/components/SessionActionsMenu";
import { Chevron } from "@/components/ui/chevron";
import { LockIcon } from "@/components/ui/lock-icon";
import {
  AttentionIndicator,
  ContextIcon,
  sessionContext,
} from "./SessionContext";
import { useNotifications, notificationSender } from "@/lib/useNotifications";
import { privateSessionTitle } from "./PrivateSessionCard";
import {
  listSessions,
  withoutReadiness,
  type SessionEntry,
} from "@/lib/privateChannel";
import { usePairedHosts } from "@/lib/hostPairing";
import { isLocalHost } from "@/lib/localHost";
import { filterClosing } from "@/lib/pendingClose";
import { useLiveSwitchHotkey } from "@/lib/useLiveSwitchHotkey";
import type { AgentReadiness, LiveSession } from "@/lib/types";

/** Basename of a filesystem path (last non-empty segment), or "" if none. */
function basename(path: string | null | undefined): string {
  if (!path) return "";
  const parts = path.split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : "";
}

/** A short, human label for a live session tab. */
function tabLabel(s: LiveSession): string {
  const title = s.title?.trim();
  if (title) return title;
  const base = basename(s.project_path);
  if (base) return base;
  // Fall back to a truncated id so the tab is never blank.
  return s.id.length > 8 ? `${s.id.slice(0, 8)}…` : s.id;
}

interface LiveTabsProps {
  /** All of the viewer's currently-active live sessions (from `GET /api/live`). */
  sessions: LiveSession[];
  /** The `:id` of the live session currently displayed. */
  currentId: string;
}

/**
 * A tab bar of the viewer's active live sessions. Clicking (or arrow-navigating
 * to + Enter/Space on) a tab switches the displayed live session by navigating
 * to `/sessions/<id>`. The current `:id` is highlighted (when it's one of them).
 *
 * Renders whenever there's at least one live session OTHER than the one being
 * viewed — i.e. something to switch to. On a live session that's the only one
 * there's nothing to switch to (hidden); but from a STORED session (whose id is
 * never in the live set) even a single live session is worth surfacing on top
 * as a jump target.
 *
 * Switching preserves the `?fullscreen=1` flag so a fullscreen switch stays
 * fullscreen (the in-overlay switcher relies on this). The first nine tabs show
 * their `Ctrl-A <n>` index — the same shortcut `useLiveSwitchHotkey` listens for.
 *
 * Every tab carries a ✕. On a tab the viewer authored it ENDS the session
 * (`POST /api/live/{id}/complete`, with a confirm) — graduating it and reaping
 * the owning `claude` process. On a not-mine tab (a session shared with the
 * viewer) it SOFT-CLOSES the session (`POST /api/live/{id}/dismiss`) — the
 * session drops out of the live list and graduates to a stored one, but the
 * owner's process keeps running. Reachable from the page tabs and the fullscreen
 * overlay switcher alike.
 */
export function LiveTabs({ sessions, currentId }: LiveTabsProps) {
  const { notifications, canRequest } = useNotifications();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const { end, endingId } = useEndLiveSession(sessions, currentId);
  const { dismiss, dismissingId } = useDismissLiveSession(sessions, currentId);
  const [contextTarget, setContextTarget] = React.useState<{
    session: LiveSession;
    x: number;
    y: number;
  } | null>(null);
  const tabs = (): SessionTabItem[] =>
    sessions.map((session) => {
      const context = sessionContext(notifications, session.id);
      const isAuthor = session.viewer_is_author;
      const label = tabLabel(session);
      return {
        id: session.id,
        label,
        agent: session.agent,
        cli_attached: session.cli_attached,
        cwd: session.project_path,
        context,
        title: tabTitle(
          label,
          session.project_path,
          isAuthor,
          context.attention.map(
            (item) => `${notificationSender(item)}: ${item.message}`,
          ),
        ),
        onContext:
          isAuthor || canRequest
            ? (event) => {
                event.preventDefault();
                setContextTarget({
                  session,
                  x: event.clientX,
                  y: event.clientY,
                });
              }
            : undefined,
        close: {
          label: `${isAuthor ? "End" : "Close"} ${tabLabel(session)}`,
          title: isAuthor ? "End session" : "Close (saved to sessions)",
          pending: isAuthor
            ? endingId === session.id
            : dismissingId === session.id,
          run: () => {
            if (isAuthor) void end(session.id);
            else void dismiss(session.id);
          },
        },
      };
    });
  return (
    <>
      <SessionTabs
        items={tabs}
        currentId={currentId}
        onSelect={(id) =>
          navigate(
            `/sessions/${encodeURIComponent(id)}${searchParams.get("fullscreen") === "1" ? "?fullscreen=1" : ""}`,
          )
        }
        onReorder={async (id, beforeId) => {
          await api.reorderLiveSession(id, beforeId);
          await queryClient.invalidateQueries({
            queryKey: LIVE_SESSIONS_QUERY_KEY,
          });
        }}
      />
      {contextTarget && (
        <SessionActionsMenu
          key={contextTarget.session.id}
          session={{
            ...contextTarget.session,
            title: tabLabel(contextTarget.session),
          }}
          sessions={sessions}
          currentId={currentId}
          context={contextTarget}
          onContextClose={() => setContextTarget(null)}
        />
      )}
    </>
  );
}

interface SessionTabItem {
  id: string;
  label: string;
  agent?: string;
  cli_attached?: boolean;
  readiness?: AgentReadiness;
  /** Working directory the agent is running against; rendered as the tab tooltip. */
  cwd?: string | null;
  /** True for private tabs (host-only channel); surfaces a lock icon on the tab. */
  isPrivate?: boolean;
  hostLabel?: string;
  title?: string;
  context?: ReturnType<typeof sessionContext>;
  onContext?: (event: React.MouseEvent) => void;
  close?: { label: string; title: string; pending: boolean; run: () => void };
}

/** Compose a tab tooltip: attention message wins; otherwise identity + cwd + hint. */
function tabTitle(
  label: string,
  cwd: string | null | undefined,
  isAuthor: boolean,
  attentionMessages: string[],
): string {
  const attention = attentionMessages.join("\n\n");
  if (attention) return attention;
  const lines: string[] = [label];
  if (cwd) lines.push(cwd);
  if (isAuthor) lines.push("Right-click to rename, set project, or share");
  return lines.join("\n");
}

/** Shared presentation and interactions; adapters supply routes and real actions. */
function SessionTabs({
  items,
  currentId,
  onSelect,
  onReorder,
}: {
  items: () => SessionTabItem[];
  currentId: string;
  onSelect: (id: string) => void;
  onReorder: (id: string, beforeId: string | null) => Promise<void>;
}) {
  const savedSessions = items();
  const draggedId = React.useRef<string | null>(null);
  const lastDragPoint = React.useRef<string | null>(null);
  const [preview, setPreview] = React.useState<string[] | null>(null);
  const sessions = preview
    ? [...savedSessions].sort((a, b) => {
        const left = preview.indexOf(a.id);
        const right = preview.indexOf(b.id);
        return (
          (left < 0 ? preview.length : left) -
          (right < 0 ? preview.length : right)
        );
      })
    : savedSessions;
  const reorder = useMutation({
    mutationFn: ({ id, beforeId }: { id: string; beforeId: string | null }) =>
      onReorder(id, beforeId),
    onSuccess: () => setPreview(null),
    onError: () => setPreview(null),
  });
  function move(index: number, direction: -1 | 1) {
    if (reorder.isPending) return;
    const target =
      direction === -1
        ? sessions[index - 1]?.id
        : (sessions[index + 2]?.id ?? null);
    reorder.mutate({ id: sessions[index].id, beforeId: target });
  }
  const [mobileOpen, setMobileOpen] = React.useState(false);
  const tablistId = React.useId();
  const tabRefs = React.useRef<(HTMLButtonElement | null)[]>([]);

  // The tabs are the session identity now (name + activity dot + private
  // marker) — they replace the old big session header — so a single tab must
  // still render. Only hide when the viewer has no live sessions at all.
  if (sessions.length === 0) return null;

  const currentIndex = Math.max(
    0,
    sessions.findIndex((s) => s.id === currentId),
  );

  function focusTab(index: number) {
    const clamped = (index + sessions.length) % sessions.length;
    tabRefs.current[clamped]?.focus();
  }

  function onKeyDown(e: React.KeyboardEvent, index: number) {
    if (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      e.preventDefault();
      const direction = e.key === "ArrowLeft" ? -1 : 1;
      if (index + direction >= 0 && index + direction < sessions.length)
        move(index, direction);
      return;
    }
    switch (e.key) {
      case "ArrowRight":
      case "ArrowDown":
        e.preventDefault();
        focusTab(index + 1);
        break;
      case "ArrowLeft":
      case "ArrowUp":
        e.preventDefault();
        focusTab(index - 1);
        break;
      case "Home":
        e.preventDefault();
        focusTab(0);
        break;
      case "End":
        e.preventDefault();
        focusTab(sessions.length - 1);
        break;
    }
  }

  return (
    <>
      <div className="max-w-full">
        <button
          type="button"
          aria-controls={tablistId}
          aria-expanded={mobileOpen}
          aria-label={`${mobileOpen ? "Hide" : "Show"} live sessions`}
          onClick={() => setMobileOpen((open) => !open)}
          className="flex w-full items-center justify-between gap-3 rounded-lg border border-border bg-muted/40 px-3 py-2 text-sm font-medium text-foreground transition-colors hover:bg-muted md:hidden"
        >
          <span className="min-w-0 truncate">
            Live sessions
            <span className="ml-2 text-xs font-normal text-muted-foreground">
              {sessions.length}
            </span>
          </span>
          <Chevron open={mobileOpen} />
        </button>
        <div
          id={tablistId}
          role="tablist"
          aria-label="Live sessions"
          onDragOver={(event) => {
            if (draggedId.current) event.preventDefault();
          }}
          onDrop={(event) => {
            const id = draggedId.current;
            if (!id || !preview || reorder.isPending) return;
            event.preventDefault();
            draggedId.current = null;
            const index = sessions.findIndex((session) => session.id === id);
            reorder.mutate({ id, beforeId: sessions[index + 1]?.id ?? null });
          }}
          className={cn(
            "max-w-full flex-wrap gap-1 rounded-lg border border-border bg-muted/40 p-1 md:inline-flex",
            mobileOpen ? "mt-2 flex" : "hidden",
            "md:mt-0",
          )}
        >
          {sessions.map((s, i) => {
            const active = s.id === currentId;
            const { attention, share } = s.context ?? {
              attention: [],
              share: null,
            };
            return (
              <div
                key={s.id}
                className={cn(
                  "relative inline-flex max-w-[20rem] items-center rounded-md transition-colors cursor-grab active:cursor-grabbing",
                  attention.length > 0 &&
                    "bg-red-500/20 text-red-700 dark:text-red-100 ring-1 ring-inset ring-red-500/60",
                  active &&
                    attention.length === 0 &&
                    "bg-emerald-500/20 text-foreground shadow-xs ring-1 ring-inset ring-emerald-500/50",
                )}
                draggable={!reorder.isPending}
                onDragStart={(event) => {
                  draggedId.current = s.id;
                  lastDragPoint.current = null;
                  setPreview(null);
                  event.dataTransfer.setData("text/plain", s.id);
                  event.dataTransfer.effectAllowed = "move";
                }}
                onDragEnd={() => {
                  // A completed drop clears the drag id but keeps its pending preview.
                  if (draggedId.current) setPreview(null);
                  draggedId.current = null;
                }}
                onDragOver={(event) => {
                  const id = draggedId.current;
                  if (!id || reorder.isPending) return;
                  event.preventDefault();
                  const point = `${event.clientX},${event.clientY}`;
                  // Moving the tabs under a stationary pointer must not flip the preview.
                  if (lastDragPoint.current === point) return;
                  lastDragPoint.current = point;
                  if (id === s.id) return;
                  const bounds = event.currentTarget.getBoundingClientRect();
                  const after = event.clientX >= bounds.left + bounds.width / 2;
                  const ids = sessions
                    .map((session) => session.id)
                    .filter((value) => value !== id);
                  ids.splice(ids.indexOf(s.id) + (after ? 1 : 0), 0, id);
                  setPreview(ids);
                }}
                onDrop={(event) => {
                  event.preventDefault();
                  const id = draggedId.current;
                  draggedId.current = null;
                  if (!id || reorder.isPending) return;
                  if (preview) {
                    const index = sessions.findIndex(
                      (session) => session.id === id,
                    );
                    reorder.mutate({
                      id,
                      beforeId: sessions[index + 1]?.id ?? null,
                    });
                    return;
                  }
                  if (id === s.id) return;
                  const bounds = event.currentTarget.getBoundingClientRect();
                  const after = event.clientX >= bounds.left + bounds.width / 2;
                  const beforeId = after ? (sessions[i + 1]?.id ?? null) : s.id;
                  if (beforeId !== id) reorder.mutate({ id, beforeId });
                }}
              >
                <button
                  ref={(el) => {
                    tabRefs.current[i] = el;
                  }}
                  role="tab"
                  type="button"
                  aria-selected={active}
                  aria-description="Drag to reorder, or press Alt+Left or Alt+Right"
                  tabIndex={active || (currentIndex < 0 && i === 0) ? 0 : -1}
                  title={s.title ?? s.label}
                  onContextMenu={s.onContext}
                  onClick={() => {
                    if (!active) {
                      setMobileOpen(false);
                      onSelect(s.id);
                    }
                  }}
                  onKeyDown={(e) => onKeyDown(e, i)}
                  className={cn(
                    "inline-flex min-w-0 items-center gap-1.5 rounded-md py-1.5 pl-3 pr-1.5 text-sm font-medium transition-colors focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring",
                    active
                      ? "text-foreground"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  <AgentReadinessIndicator
                    readiness={s.readiness}
                    connected={s.cli_attached !== false}
                  />
                  {i < 9 && (
                    <kbd
                      aria-hidden
                      title={`Ctrl-A ${i + 1}`}
                      className="shrink-0 rounded border border-border/60 bg-background/60 px-1 font-mono text-[10px] leading-tight text-muted-foreground"
                    >
                      {i + 1}
                    </kbd>
                  )}
                  {s.isPrivate && (
                    <LockIcon
                      label="Private session"
                      title="Private session — stays on your host"
                      className="text-muted-foreground"
                    />
                  )}
                  {s.agent === "codex" && (
                    <span
                      aria-label="Codex session"
                      title="Codex session"
                      className="shrink-0 font-mono text-[11px] leading-none text-violet-500 dark:text-violet-300"
                    >
                      ✦
                    </span>
                  )}
                  {s.agent === "opencode" && (
                    <span
                      aria-label="OpenCode session"
                      title="OpenCode session"
                      className="shrink-0 font-mono text-[11px] leading-none text-sky-600 dark:text-sky-300"
                    >
                      ◇
                    </span>
                  )}
                  <span className="truncate">{s.label}</span>
                  {s.hostLabel && (
                    <span
                      title={s.hostLabel}
                      aria-label={`Host: ${s.hostLabel}`}
                      className="max-w-16 shrink-0 truncate rounded bg-muted px-1 text-[10px] text-muted-foreground"
                    >
                      {s.hostLabel}
                    </span>
                  )}
                </button>
                {share && (
                  <ContextIcon
                    message={`${notificationSender(share)}: ${share.message}`}
                  />
                )}
                <AttentionIndicator requests={attention} />
                {s.close && (
                  <button
                    type="button"
                    aria-label={s.close.label}
                    title={s.close.title}
                    disabled={s.close.pending}
                    onClick={(e) => {
                      e.stopPropagation();
                      s.close?.run();
                    }}
                    className="mr-1 flex h-4 w-4 shrink-0 items-center justify-center rounded text-muted-foreground/70 transition-colors hover:bg-foreground/10 hover:text-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50"
                  >
                    <span aria-hidden className="text-sm leading-none">
                      ×
                    </span>
                  </button>
                )}
              </div>
            );
          })}
        </div>
      </div>
      {reorder.error && (
        <p role="alert" className="text-sm text-destructive">
          {reorder.error.message}
        </p>
      )}
    </>
  );
}

/** Read each paired host independently without prompting for access. */
export function AllHostSessionTabs(props: {
  sessions: SessionEntry[];
  hostId: string;
  currentId: string;
}) {
  const { hosts } = usePairedHosts();
  const others = isLocalHost()
    ? []
    : hosts.filter((host) => host.id !== props.hostId);
  const queries = useQueries({
    queries: others.map((host) => ({
      queryKey: ["private", host.id, "active-sessions"],
      queryFn: async () =>
        filterClosing(
          privateScope(host.id),
          await hostControl.privateRead(
            host.id,
            (channel) => listSessions(channel, { runningOnly: true }),
            { allowUnlock: false },
          ),
        ),
      retry: false,
      refetchInterval: 5_000,
    })),
  });
  return (
    <PrivateSessionTabs
      {...props}
      otherHosts={others.map((host, i) => ({
        hostId: host.id,
        label: host.label,
        sessions: queries[i].isError
          ? withoutReadiness(queries[i].data ?? [])
          : (queries[i].data ?? []),
      }))}
      hostLabel={hosts.find((host) => host.id === props.hostId)?.label}
    />
  );
}

/** Host-list adapter: no fabricated remote session or central mutation authority. */
export function PrivateSessionTabs({
  sessions,
  hostId,
  currentId,
  otherHosts = [],
  hostLabel,
}: {
  otherHosts?: { hostId: string; label: string; sessions: SessionEntry[] }[];
  hostLabel?: string;
  sessions: SessionEntry[];
  hostId: string;
  currentId: string;
}) {
  const multiHost = !!hostLabel || otherHosts.length > 0;
  const currentKey = multiHost ? `${hostId}:${currentId}` : currentId;
  const entries = [
    ...sessions.map((session) => ({
      ...session,
      id: multiHost ? `${hostId}:${session.id}` : session.id,
      tabHost: hostId,
      hostLabel,
      nativeId: session.id,
    })),
    ...otherHosts.flatMap((host) =>
      host.sessions.map((session) => ({
        ...session,
        id: `${host.hostId}:${session.id}`,
        nativeId: session.id,
        tabHost: host.hostId,
        hostLabel: host.label,
      })),
    ),
  ];
  const resolve = (id: string) => entries.find((entry) => entry.id === id);
  const { notifications } = useNotifications();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const [endingId, setEndingId] = React.useState<string | null>(null);
  const [contextTarget, setContextTarget] = React.useState<{
    session: SessionEntry;
    hostId: string;
    x: number;
    y: number;
  } | null>(null);
  async function end(id: string) {
    if (endingId) return;
    const entry = resolve(id);
    if (!entry) return;
    const hostId = entry.tabHost;
    const nativeId = entry.nativeId;
    setEndingId(id);

    // Optimistic removal from this host's caches so the tab goes away now.
    // The scope is per-host so another paired host's caches stay untouched.
    markClosing(privateScope(hostId), nativeId);
    const keys = [
      ["private", hostId, "sessions"] as const,
      ["private", hostId, "active-sessions"] as const,
    ];
    const snapshots = keys.map(
      (key) => [key, queryClient.getQueryData<SessionEntry[]>(key)] as const,
    );
    await Promise.all(
      keys.map((key) => queryClient.cancelQueries({ queryKey: key })),
    );
    for (const key of keys) {
      queryClient.setQueryData<SessionEntry[]>(key, (prev) =>
        prev?.filter((entry) => entry.id !== nativeId),
      );
    }
    if (id === currentKey) {
      const next = running.find((entry) => entry.id !== id);
      if (next) select(next.id);
    }

    try {
      await hostControl.endSession(nativeId, hostId);
      await queryClient.invalidateQueries({ queryKey: ["private", hostId] });
    } catch (error) {
      for (const [key, snapshot] of snapshots) {
        if (snapshot) queryClient.setQueryData(key, snapshot);
      }
      toast((error as Error).message, "error");
    } finally {
      clearClosing(privateScope(hostId), nativeId);
      setEndingId(null);
    }
  }
  const [searchParams] = useSearchParams();
  const storageKey = multiHost
    ? "conversations.private-tabs:all-hosts"
    : `conversations.private-tabs:${hostId}`;
  const [order, setOrder] = React.useState<string[]>(() => {
    try {
      const stored: unknown = JSON.parse(
        localStorage.getItem(storageKey) ?? "[]",
      );
      return Array.isArray(stored)
        ? stored.filter((id): id is string => typeof id === "string")
        : [];
    } catch {
      return [];
    }
  });
  const running = entries
    .filter((session) => session.running)
    .sort((a, b) => {
      const ai = order.indexOf(a.id),
        bi = order.indexOf(b.id);
      return (
        (ai < 0 ? order.length : ai) - (bi < 0 ? order.length : bi) ||
        (multiHost ? a.tabHost.localeCompare(b.tabHost) : 0)
      );
    });
  const select = (id: string) => {
    const entry = resolve(id);
    if (!entry) return;
    const params = new URLSearchParams();
    if (searchParams.get("fullscreen") === "1") params.set("fullscreen", "1");
    if (hostLabel || otherHosts.length) params.set("host", entry.tabHost);
    navigate(
      `/private/${encodeURIComponent(entry.nativeId)}${params.size ? `?${params}` : ""}`,
    );
  };
  useLiveSwitchHotkey(running, currentKey, { onSelect: select });
  return (
    <>
      <SessionTabs
        currentId={currentKey}
        items={() =>
          running.map((session) => {
            const label = privateSessionTitle(session);
            const context = sessionContext(
              notifications,
              session.nativeId,
              session.tabHost,
            );
            return {
              id: session.id,
              context,
              label,
              hostLabel: session.hostLabel,
              agent: session.agent,
              cli_attached: true,
              readiness: session.readiness,
              cwd: session.cwd,
              isPrivate: true,
              title: tabTitle(
                label,
                session.cwd,
                true,
                context.attention.map(
                  (item) => `${notificationSender(item)}: ${item.message}`,
                ),
              ),
              close: {
                label: `End ${label}`,
                title: "End session",
                pending: endingId === session.id,
                run: () => void end(session.id),
              },
              onContext: (event: React.MouseEvent) => {
                event.preventDefault();
                setContextTarget({
                  session: { ...session, id: session.nativeId },
                  hostId: session.tabHost,
                  x: event.clientX,
                  y: event.clientY,
                });
              },
            };
          })
        }
        onSelect={select}
        onReorder={async (id, beforeId) => {
          const next = running
            .map((session) => session.id)
            .filter((value) => value !== id);
          const at = beforeId === null ? next.length : next.indexOf(beforeId);
          next.splice(at < 0 ? next.length : at, 0, id);
          localStorage.setItem(storageKey, JSON.stringify(next));
          setOrder(next);
        }}
      />
      {contextTarget && (
        <PrivateSessionActionsMenu
          key={contextTarget.session.id}
          session={contextTarget.session}
          hostId={contextTarget.hostId}
          context={contextTarget}
          onContextClose={() => setContextTarget(null)}
        />
      )}
    </>
  );
}
