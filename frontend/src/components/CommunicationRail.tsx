import * as React from "react";
import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import {
  readPrivateCommunications,
  type ConversationView,
  type CommunicationPeerView,
  type CommunicationsView,
} from "@/lib/privateCommunications";
import { fullTimestamp, humanizeTime } from "@/lib/utils";
import { AgentBadge } from "./AgentBadge";

/** localStorage key for the fullscreen communication rail's collapsed state
 *  (its own preference, independent of the PR and Linear rails). */
const COMMUNICATION_RAIL_COLLAPSED_KEY =
  "conversations.communicationRailCollapsed";

export const sessionCommunicationsKey = (sessionId: string) =>
  ["session-communications", sessionId] as const;

const PEER_STATE: Record<
  CommunicationPeerView["state"],
  { label: string; title: string; className: string }
> = {
  unknown: {
    label: "Unknown",
    title: "This host has no verified current presence for this peer",
    className: "text-muted-foreground",
  },
  live: {
    label: "Live",
    title:
      "A CLI stream is attached; not proof the agent is processing messages",
    className: "text-emerald-600 dark:text-emerald-400",
  },
  idle: {
    label: "Idle",
    title:
      "Registered live but nothing is attached; a message waits until its client fetches it",
    className: "text-amber-600 dark:text-amber-400",
  },
  ended: {
    label: "Ended",
    title: "No longer running; only its owner can reopen it",
    className: "text-muted-foreground",
  },
};

/** Literal wording for a sent message's status. None of these prove the
 *  recipient acted on it. */
const SENT_STATUS: Record<string, { label: string; title: string }> = {
  prepared: {
    label: "prepared",
    title: "Prepared for delivery; delivery is not confirmed",
  },
  uncertain: { label: "uncertain", title: "Delivery outcome is unknown" },
  refused: { label: "refused", title: "The recipient refused delivery" },
  revoked: { label: "revoked", title: "Delivery authority was revoked" },
  expired: { label: "expired", title: "The delivery request expired" },
  queued: {
    label: "queued",
    title: "Stored by the server; the recipient has not fetched it yet",
  },
  received: {
    label: "fetched",
    title: "The recipient's client fetched it; not proof it was processed",
  },
  acknowledged: {
    label: "acknowledged",
    title: "The recipient accepted or replied to it; not proof work completed",
  },
};

/**
 * Compact, fullscreen-only rail listing the sessions this session can message,
 * below the PR and Linear rails. Sessions it has actually exchanged messages
 * with are listed under "Conversations" (always, whatever their state); the
 * rest under "Can message" when owned by someone else and reachable. Other
 * people's ended sessions are hidden behind "Show all".
 *
 * The data is owner-only: anyone else (including people who can read this
 * transcript) gets a 403 and the rail renders nothing. A peer's title links to
 * its transcript only when the server reports the viewer can read it —
 * messaging authority never implies transcript access. Renders nothing while
 * loading or when there are no peers, so it only claims width when useful.
 */
export function CommunicationRail({
  sessionId,
  hostId,
}: {
  sessionId: string;
  hostId?: string;
}) {
  const { data, dataUpdatedAt, error, refetch } = useQuery({
    queryKey: hostId
      ? ["private", hostId, ...sessionCommunicationsKey(sessionId)]
      : sessionCommunicationsKey(sessionId),
    queryFn: async (): Promise<CommunicationsView> => {
      if (hostId) return readPrivateCommunications(hostId, sessionId);
      const data = await api.listSessionCommunications(sessionId);
      return {
        ...data,
        sessions: data.sessions.map((peer) => ({
          ...peer,
          host_id: null,
          href: peer.readable
            ? `/sessions/${encodeURIComponent(peer.session_id)}`
            : undefined,
        })),
      };
    },
    retry: false,
    refetchInterval: 15_000,
  });

  // Unlike the PR and Linear rails, almost every project session has peers, so
  // on a phone the expanded rail would leave the terminal ~100px wide. Start
  // collapsed there until the viewer picks a state.
  const [collapsed, setCollapsed] = React.useState<boolean>(() => {
    try {
      const stored = localStorage.getItem(COMMUNICATION_RAIL_COLLAPSED_KEY);
      if (stored !== null) return stored === "true";
    } catch {
      // fall through to the viewport default
    }
    return (
      typeof window.matchMedia === "function" &&
      window.matchMedia("(max-width: 767px)").matches
    );
  });
  const toggleCollapsed = React.useCallback(() => {
    setCollapsed((c) => {
      const next = !c;
      try {
        localStorage.setItem(COMMUNICATION_RAIL_COLLAPSED_KEY, String(next));
      } catch {
        // ignore — quota or private-browsing restriction
      }
      return next;
    });
  }, []);

  const [showAll, setShowAll] = React.useState(false);

  if (hostId && error)
    return (
      <aside
        aria-label="Communication"
        className={`rounded-lg border border-border bg-card/60 p-3 text-xs ${collapsed ? "w-9" : "w-60"}`}
      >
        <button
          type="button"
          aria-expanded={!collapsed}
          aria-label={
            collapsed ? "Expand communication" : "Collapse communication"
          }
          title="Communication unavailable"
          onClick={toggleCollapsed}
        >
          {collapsed ? "!" : "Communication"}
        </button>
        {!collapsed && (
          <>
            <p role="alert">Communication unavailable: {error.message}</p>
            <button className="underline" onClick={() => void refetch()}>
              Retry
            </button>
          </>
        )}
      </aside>
    );
  if (!data || data.sessions.length === 0) return null;

  const conversations = data.sessions.filter((s) => s.conversation);
  const eligible = hostId
    ? []
    : data.sessions.filter(
        (s) =>
          !s.conversation &&
          s.reachable &&
          (!data.owner_email || s.owner_email !== data.owner_email),
      );
  const unreachable = hostId
    ? []
    : data.sessions.filter(
        (s) =>
          !s.conversation &&
          !s.reachable &&
          s.last_seen_at !== null &&
          Date.parse(s.last_seen_at) >= dataUpdatedAt - 24 * 60 * 60 * 1000 &&
          (!data.owner_email || s.owner_email !== data.owner_email),
      );
  const visibleCount = conversations.length + eligible.length;
  if (visibleCount === 0 && unreachable.length === 0) return null;

  return (
    <aside
      aria-label="Communication"
      className={`flex min-h-0 flex-col rounded-lg border border-border bg-card/60 p-3 ${
        collapsed ? "w-9 items-center" : "w-60"
      }`}
    >
      <button
        type="button"
        aria-expanded={!collapsed}
        aria-label={
          collapsed ? "Expand communication" : "Collapse communication"
        }
        onClick={toggleCollapsed}
        title={collapsed ? "Expand communication" : "Collapse communication"}
        className={`flex min-w-0 items-center gap-1 text-left ${collapsed ? "" : "mb-2"}`}
      >
        <span
          aria-hidden
          className={`text-[0.65rem] text-muted-foreground transition-transform duration-150 ${
            collapsed ? "" : "rotate-90"
          }`}
        >
          ▶
        </span>
        {!collapsed && (
          <span className="truncate text-xs font-semibold text-muted-foreground">
            Communication
          </span>
        )}
      </button>

      {collapsed ? (
        <button
          type="button"
          aria-label={`Expand communication (${visibleCount})`}
          onClick={toggleCollapsed}
          className="mt-1 text-xs font-semibold text-muted-foreground transition-colors hover:text-foreground"
        >
          {visibleCount}
        </button>
      ) : (
        <div className="min-h-0 space-y-3 overflow-y-auto">
          {conversations.length > 0 && (
            <RailSection
              label="Conversations"
              peers={conversations}
              viewer={data.owner_email ?? null}
            />
          )}
          {eligible.length > 0 && (
            <RailSection
              label="Can message"
              peers={eligible}
              viewer={data.owner_email ?? null}
            />
          )}
          {showAll && unreachable.length > 0 && (
            <RailSection
              label="Not reachable"
              peers={unreachable}
              viewer={data.owner_email ?? null}
            />
          )}
          {unreachable.length > 0 && (
            <button
              type="button"
              onClick={() => setShowAll((v) => !v)}
              className="text-[0.65rem] text-muted-foreground hover:text-foreground hover:underline"
            >
              {showAll ? "Hide ended" : `Show all (${unreachable.length})`}
            </button>
          )}
        </div>
      )}
    </aside>
  );
}

function RailSection({
  label,
  peers,
  viewer,
}: {
  label: string;
  peers: CommunicationPeerView[];
  viewer: string | null;
}) {
  return (
    <section aria-label={label}>
      <h3 className="mb-1 text-[0.65rem] font-semibold uppercase tracking-wide text-muted-foreground">
        {label}
      </h3>
      <ul className="space-y-2">
        {peers.map((peer) => (
          <RailPeerRow
            key={`${peer.host_id ?? "local"}:${peer.session_id}`}
            peer={peer}
            viewer={viewer}
          />
        ))}
      </ul>
    </section>
  );
}

/** One peer: title (a transcript link only with read access), agent, other
 *  owner, then either message metadata or when it was last seen. */
function RailPeerRow({
  peer,
  viewer,
}: {
  peer: CommunicationPeerView;
  viewer: string | null;
}) {
  const conversation = peer.conversation;
  const href = peer.href;
  return (
    <li
      className="space-y-0.5 text-xs"
      data-testid={`comm-row-${peer.session_id}`}
    >
      <div className="flex min-w-0 items-center gap-1.5">
        {peer.readable && href ? (
          <Link
            to={href}
            title={peer.title}
            className="truncate font-medium text-foreground hover:text-primary hover:underline"
          >
            {peer.title}
          </Link>
        ) : (
          <span title={peer.title} className="truncate font-medium">
            {peer.title}
          </span>
        )}
        {peer.agent && (
          <AgentBadge agent={peer.agent} className="px-1 py-0 text-[0.6rem]" />
        )}
      </div>
      {peer.owner_email && peer.owner_email !== viewer && (
        <div
          className="truncate text-muted-foreground"
          title={peer.owner_email}
        >
          {peer.owner_email}
        </div>
      )}
      <div className="text-muted-foreground">
        <span
          className={`font-medium ${PEER_STATE[peer.state].className}`}
          title={PEER_STATE[peer.state].title}
        >
          {PEER_STATE[peer.state].label}
        </span>
        {!conversation && peer.last_seen_at && (
          <span
            title={`Last live activity or upload: ${fullTimestamp(peer.last_seen_at)}`}
          >
            {" · "}last seen {humanizeTime(peer.last_seen_at)}
          </span>
        )}
      </div>
      {conversation && <ConversationLines conversation={conversation} />}
      {!peer.readable && (
        <div
          className="text-muted-foreground"
          title="Messaging does not grant transcript access"
        >
          No transcript access
        </div>
      )}
    </li>
  );
}

function ConversationLines({
  conversation,
}: {
  conversation: ConversationView;
}) {
  const status = conversation.last_sent_status
    ? (SENT_STATUS[conversation.last_sent_status] ?? {
        label: conversation.last_sent_status,
        title: "Recorded delivery state",
      })
    : null;
  return (
    <>
      <div className="text-muted-foreground">
        {conversation.sent_count} sent · {conversation.received_count} received
      </div>
      {conversation.last_message_at && (
        <div
          className="text-muted-foreground"
          title={fullTimestamp(conversation.last_message_at)}
        >
          {conversation.last_direction === "sent" ? "↑" : "↓"} last message{" "}
          {humanizeTime(conversation.last_message_at)}
        </div>
      )}
      {status && (
        <div className="text-muted-foreground" title={status.title}>
          last sent: {status.label}
        </div>
      )}
      {conversation.messages?.length > 0 && (
        <ul
          aria-label="Exchanged messages"
          className="list-disc space-y-0.5 pl-3 text-[0.65rem] text-muted-foreground"
        >
          {conversation.messages.map((message) => (
            <li
              key={message.id}
              title={`${message.direction === "sent" ? "Sent" : "Received"} · ${message.created_at ? fullTimestamp(message.created_at) : "Time unknown"}`}
            >
              <span className="line-clamp-2 break-words">{message.title}</span>
              {message.status && (
                <span className="text-muted-foreground">
                  {SENT_STATUS[message.status]?.label ?? message.status}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
      {conversation.unacknowledged_incoming > 0 && (
        <div
          className="font-medium text-amber-600 dark:text-amber-400"
          title="Incoming messages this session has not acknowledged"
        >
          {conversation.unacknowledged_incoming} not acknowledged
        </div>
      )}
    </>
  );
}
