import { exactTitleFirst } from "@/lib/sessionSearchRank";
import { useLocalHost } from "@/lib/localHost";
import * as React from "react";
import { useHostCollections } from "@/lib/useHostCollections";
import {
  PrivateSessionCard,
  privateSessionTitle,
} from "@/components/PrivateSessionCard";
import type { SessionEntry } from "@/lib/privateChannel";
import type { Summary } from "@/lib/types";
import { Button } from "@/components/ui/button";
import { Link, useSearchParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { HostControlError, unlockAgain } from "@/lib/hostControl";
import { api } from "@/lib/api";
import { useDebounce } from "@/lib/useDebounce";
import { useAuth, useRedirectOnUnauthorized } from "@/lib/useAuth";
import {
  asScope,
  filterByScope,
  scopeCounts,
  type SessionScope,
} from "@/lib/sessionScope";
import { useLiveSessions } from "@/lib/useLiveSessions";
import {
  liveMatchesQuery,
  liveMatchesScope,
  liveToSummary,
} from "@/lib/liveSearch";
import { SessionCollectionToolbar } from "@/components/SessionCollectionToolbar";
import { DEFAULT_SORT, type SortOrder } from "@/components/SortControl";
import { SessionCard } from "@/components/SessionCard";
import { ScopeTabs } from "@/components/ScopeTabs";
import { StartNewSessionButton } from "@/components/HostActions";
import { EmptyState, ErrorState, Spinner } from "@/components/states";

const EMPTY_HINTS: Record<SessionScope, { title: string; message: string }> = {
  all: {
    title: "No sessions yet",
    message:
      "Upload sessions with the conversations CLI to see them here.",
  },
  mine: {
    title: "You haven’t uploaded any sessions",
    message: "Sessions you upload while signed in will appear here.",
  },
  shared: {
    title: "Nothing shared with you",
    message: "Sessions others share with you directly will appear here.",
  },
  org: {
    title: "No organization sessions",
    message: "Organization-wide sessions from your teammates will appear here.",
  },
};

export function SessionList() {
  const local = useLocalHost();
  return <SessionCollection local={local.local} />;
}

function SessionCollection({ local }: { local: boolean }) {
  const collections = useHostCollections();
  const privateQuery = {
    data: collections.flatMap((collection) => collection.data),
    isLoading: collections.some((collection) => collection.isLoading),
  };
  const [limit, setLimit] = React.useState(48);
  const [showEmpty, setShowEmpty] = React.useState(false);
  const [search, setSearch] = React.useState("");
  const [project, setProject] = React.useState("");
  const [sort, setSort] = React.useState<SortOrder>(DEFAULT_SORT);
  const debouncedSearch = useDebounce(search, 300);
  const [searchParams, setSearchParams] = useSearchParams();
  const { user } = useAuth();

  const scope = asScope(searchParams.get("scope"));
  const setScope = (next: SessionScope) =>
    setSearchParams(
      (prev) => {
        const sp = new URLSearchParams(prev);
        if (next === "all") sp.delete("scope");
        else sp.set("scope", next);
        return sp;
      },
      { replace: true },
    );

  const projectsQuery = useQuery({
    queryKey: ["projects"],
    queryFn: api.listProjects,
    enabled: !local,
  });

  const sessionsQuery = useQuery({
    queryKey: ["sessions", debouncedSearch, project],
    queryFn: () => api.listSessions({ q: debouncedSearch, project }),
    enabled: !local,
  });

  useRedirectOnUnauthorized(sessionsQuery.error);

  const { list: liveSessions, activeIds: liveIds } = useLiveSessions(!local);

  const all = local ? [] : (sessionsQuery.data ?? []);
  // Identity-relative tabs only make sense when we know who "me" is.
  const showTabs = !local && !!user;
  const counts = user ? scopeCounts(all, user.email) : null;
  const visible = sortSessions(
    filterByScope(all, scope, user?.email ?? null),
    sort,
  );
  const hint = EMPTY_HINTS[showTabs ? scope : "all"];

  // One unified list: live sessions render as LIVE cards at the top, then the
  // stored sessions. The live set comes from the live poll (it correctly scopes
  // to sessions the viewer can access — author's own plus shared) and is the
  // source of truth for "currently live"; matching live sessions are filtered
  // by the same query when searching. There is no separate "Live now" box.
  const isFiltering = !!debouncedSearch || !!project;
  // Respect the active scope tab the same way the stored list does (a live
  // session shared with me must not show on "My sessions"). Scope is derived
  // from the live session itself, so a brand-new live card is scoped without
  // waiting for the stored-list query to refetch. Scope tabs only show when
  // signed in; without an identity every live session is in scope.
  const liveScoped = showTabs
    ? liveSessions.filter((s) => liveMatchesScope(s, scope))
    : liveSessions;
  const liveCards = isFiltering
    ? liveScoped.filter((s) => liveMatchesQuery(s, debouncedSearch, project))
    : liveScoped;
  const liveCardIds = new Set(liveCards.map((s) => s.id));
  // Drop any stored row already shown as a live card (the eager `sessions` row
  // shares the live id) so a live session appears exactly once.
  const storedVisible = visible.filter((s) => !liveCardIds.has(s.id));
  type Item =
    | {
        source: "private";
        session: SessionEntry & { hostId: string; hostLabel: string };
      }
    | { source: "remote"; session: Summary };
  const terms = debouncedSearch
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  const privateItems = (privateQuery.data ?? []).filter(
    (entry) =>
      (local || scope === "all") &&
      (!project || entry.cwd === project) &&
      terms.every((term) =>
        `${privateSessionTitle(entry)} ${entry.cwd} ${entry.agent} ${(entry.prs ?? []).join(" ")}`
          .toLowerCase()
          .includes(term),
      ) &&
      (showEmpty || entry.running || entry.messages > 0 || terms.length > 0),
  );
  // Only eligible private rows suppress a shared copy: central scope and
  // filter decisions remain independent of local possession of a transcript.
  const privateIds = new Set(privateItems.map((entry) => entry.id));
  const sharedIds = new Set(
    [...liveCards, ...storedVisible].map((entry) => entry.id),
  );
  const combined: Item[] = [
    ...liveCards
      .filter((session) => !privateIds.has(session.id))
      .map((session): Item => ({
        source: "remote",
        session: liveToSummary(session),
      })),
    ...storedVisible
      .filter((session) => !privateIds.has(session.id))
      .map((session): Item => ({ source: "remote", session })),
    ...privateItems.map((session): Item => ({ source: "private", session })),
  ];
  const title = (item: Item) =>
    item.source === "private"
      ? privateSessionTitle(item.session)
      : item.session.title;
  const running = (item: Item) =>
    item.source === "private"
      ? item.session.running
      : liveCardIds.has(item.session.id) ||
        liveIds.has(item.session.id) ||
        item.session.is_live === true;
  const time = (item: Item) =>
    item.source === "private"
      ? parseTime(item.session.last_activity)
      : sessionTime(item.session, sort);
  const direction = sort.endsWith("_asc") ? 1 : -1;
  combined.sort(
    (a, b) =>
      (!terms.length ? Number(running(b)) - Number(running(a)) : 0) ||
      direction * (time(a) - time(b)),
  );
  const ranked = exactTitleFirst(combined, debouncedSearch, title);
  const projects = new Map(
    (local ? [] : (projectsQuery.data ?? [])).map((entry) => [
      entry.project_path,
      entry.session_count,
    ]),
  );
  for (const entry of privateQuery.data ?? [])
    projects.set(entry.cwd, (projects.get(entry.cwd) ?? 0) + 1);
  const hasResults = ranked.length > 0;
  React.useEffect(() => setLimit(48), [debouncedSearch, project, sort, scope]);

  return (
    <div className="space-y-5">
      <SessionCollectionHeader />
      {!local && !collections.length && (
        <div className="rounded-xl border border-primary/30 bg-primary/5 p-4 text-sm">
          <h2 className="font-semibold">Connect a machine</h2>
          <p className="mt-1 text-muted-foreground">
            Connect your machine in Settings, then approve this browser on that
            machine to see your private sessions.
          </p>
          <Link
            to="/settings"
            className="mt-3 inline-block rounded-md border border-border px-3 py-1.5 hover:bg-accent"
          >
            Set up access
          </Link>
        </div>
      )}
      {collections.map((collection) => (
        <HostCollectionStatus
          key={collection.host.id}
          collection={collection}
        />
      ))}

      {showTabs && (
        <ScopeTabs scope={scope} counts={counts} onChange={setScope} />
      )}

      <SessionCollectionToolbar
        search={search}
        onSearch={setSearch}
        project={project}
        onProject={setProject}
        projects={Array.from(projects, ([project_path, session_count]) => ({
          project_path,
          session_count,
        }))}
        sort={sort}
        onSort={setSort}
        sortOptions={
          privateQuery.data?.length
            ? ["activity_desc", "activity_asc"]
            : undefined
        }
      />

      {!local && sessionsQuery.isError && (
        <ErrorState
          message={(sessionsQuery.error as Error)?.message}
          onRetry={() => sessionsQuery.refetch()}
        />
      )}
      {!hasResults &&
        ((!local && sessionsQuery.isLoading) || privateQuery.isLoading) && (
          <Spinner label="Loading sessions…" />
        )}
      {!hasResults &&
        !(!local && sessionsQuery.isLoading) &&
        !privateQuery.isLoading && (
          <EmptyState
            title={isFiltering ? "No matching sessions" : hint.title}
            message={
              isFiltering
                ? "Try a different search or project filter."
                : hint.message
            }
          />
        )}
      {hasResults && (
        <div className="grid items-start gap-3 sm:grid-cols-2">
          {ranked.slice(0, limit).map((item) =>
            item.source === "private" ? (
              <div
                key={`private:${item.session.hostId}:${item.session.id}`}
                className="min-w-0"
              >
                <PrivateSessionCard
                  session={item.session}
                  hostId={local ? undefined : item.session.hostId}
                  hostLabel={item.session.hostLabel}
                />
                {sharedIds.has(item.session.id) && (
                  <Link
                    className="mt-1 inline-block text-xs text-muted-foreground underline"
                    to={`/sessions/${encodeURIComponent(item.session.id)}`}
                  >
                    View shared copy
                  </Link>
                )}
              </div>
            ) : (
              <SessionCard
                key={`remote:${item.session.id}`}
                session={item.session}
                live={running(item)}
              />
            ),
          )}
        </div>
      )}
      {!showEmpty &&
        !terms.length &&
        (privateQuery.data ?? []).some(
          (entry) => !entry.running && entry.messages === 0,
        ) && (
          <Button variant="outline" onClick={() => setShowEmpty(true)}>
            Show stopped sessions without messages (
            {
              (privateQuery.data ?? []).filter(
                (entry) => !entry.running && entry.messages === 0,
              ).length
            }
            )
          </Button>
        )}
      {ranked.length > limit && (
        <Button
          variant="outline"
          onClick={() => setLimit((value) => value + 48)}
        >
          Show more sessions ({ranked.length - limit} remaining)
        </Button>
      )}
    </div>
  );
}

type SortableSession = {
  started_at: string | null;
  ended_at: string | null;
  uploaded_at: string;
};

/** Parse an RFC3339 timestamp to epoch ms, or `-Infinity` (sorts oldest) when
 * absent/unparseable. */
function parseTime(raw: string | null | undefined): number {
  const t = raw ? Date.parse(raw) : NaN;
  return Number.isNaN(t) ? -Infinity : t;
}

/** A session's effective timestamp for the chosen sort field.
 * - last activity: `ended_at` (last-message time), falling back to start, then
 *   upload — a live card's `ended_at` is null so it falls back to its start.
 * - created: `started_at`, falling back to upload. */
function sessionTime(s: SortableSession, order: SortOrder): number {
  if (order === "activity_desc" || order === "activity_asc") {
    const raw = s.ended_at ?? s.started_at ?? s.uploaded_at;
    return parseTime(raw);
  }
  return parseTime(s.started_at ?? s.uploaded_at);
}

/** Sort sessions by the chosen field + direction. Returns a new array. */
function sortSessions<T extends SortableSession>(
  sessions: T[],
  order: SortOrder,
): T[] {
  const dir = order === "activity_asc" || order === "created_asc" ? 1 : -1;
  return [...sessions].sort(
    (a, b) => dir * (sessionTime(a, order) - sessionTime(b, order)),
  );
}

function SessionCollectionHeader() {
  return (
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h1 className="text-xl font-semibold tracking-tight">Sessions</h1>
        <p className="text-sm text-muted-foreground">
          Sessions from your connected machines and shared conversation history.
        </p>
      </div>
      <StartNewSessionButton />
    </div>
  );
}

function HostCollectionStatus({
  collection,
}: {
  collection: ReturnType<typeof useHostCollections>[number];
}) {
  const { host, needsRepair, locked, access, online } = collection;
  const hostId = host.id;
  const cache = useQueryClient();
  const [unlocking, setUnlocking] = React.useState(false);
  const [unlockError, setUnlockError] = React.useState<string | null>(null);
  const privateError = collection.error;
  const needsAccessSetup =
    privateError instanceof HostControlError &&
    (privateError.code === "refused" || privateError.code === "not_paired");
  const unlock = async () => {
    setUnlocking(true);
    setUnlockError(null);
    try {
      await unlockAgain(hostId);
      await cache.invalidateQueries({ queryKey: ["host-delegation", hostId] });
    } catch (error) {
      setUnlockError((error as Error).message);
    } finally {
      setUnlocking(false);
    }
  };
  if (!needsRepair && !locked && !collection.isError) return null;
  return (
    <section aria-label={`Host: ${host.label}`} className="space-y-2">
      <p className="text-sm font-medium">{host.label}</p>
      {needsRepair && (
        <div className="rounded-xl border border-border p-4 text-sm">
          <h2 className="font-semibold">Reconnect this device</h2>
          <p className="mt-1 text-muted-foreground">
            This saved connection cannot verify this machine. On that machine,
            open Settings and choose Add a device to connect again.
          </p>
        </div>
      )}
      {locked && (
        <div className="rounded-xl border border-border p-4 text-sm">
          <h2 className="font-semibold">Unlock private sessions</h2>
          <p className="mt-1 text-muted-foreground">
            Use your passkey to see private sessions. If this device has not
            been approved, approve it on that machine first.
          </p>
          {online.data === false && (
            <p role="status" className="mt-2 text-muted-foreground">
              This machine is offline. Open Conversations on that machine and
              try again.
            </p>
          )}
          {access.isError && (
            <p role="alert" className="mt-2 text-destructive">
              Could not read this device's saved access.{" "}
              {(access.error as Error).message}
            </p>
          )}
          <Button
            className="mt-3"
            onClick={() => void unlock()}
            disabled={unlocking}
          >
            {unlocking ? "Unlocking…" : "Unlock with passkey"}
          </Button>
          {unlockError && (
            <p role="alert" className="mt-2 text-destructive">
              {unlockError}
            </p>
          )}
        </div>
      )}
      {collection.isError && (
        <ErrorState
          title={
            needsAccessSetup
              ? privateError.code === "not_paired"
                ? "No host is paired"
                : "Your host refused access"
              : "Your host is not reachable privately"
          }
          message={
            needsAccessSetup
              ? `${privateError.message} Review this browser's approval and permissions in Settings → Host access, or unlock it again with your passkey.`
              : (privateError as Error)?.message
          }
          action={
            needsAccessSetup && (
              <Link
                to="/settings"
                className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-accent"
              >
                Set up access
              </Link>
            )
          }
          onRetry={needsAccessSetup ? undefined : () => collection.refetch()}
        />
      )}
    </section>
  );
}
