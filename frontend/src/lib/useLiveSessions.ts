import { useQuery } from "@tanstack/react-query";
import { api, ApiError } from "./api";
import { LIVE_SCOPE, filterClosing } from "./pendingClose";
import type { LiveSession } from "./types";

/** Query key for the live-sessions poll. */
export const LIVE_SESSIONS_QUERY_KEY = ["live-sessions"] as const;

/** How often to re-poll `/api/live` for the active set. */
const REFETCH_INTERVAL_MS = 5000;

/**
 * Poll `/api/live` for the currently-streaming sessions.
 *
 * Returns the shared-order `list` plus an `activeIds` set keyed by `id`
 * for O(1) "is this session live?" checks on session cards. `retry: false`
 * keeps a transient blip from spamming retries between the 5s polls.
 */
export function useLiveSessions(enabled = true) {
  const query = useQuery<LiveSession[]>({
    queryKey: LIVE_SESSIONS_QUERY_KEY,
    enabled,
    queryFn: async () => {
      const [sessions, order] = await Promise.all([
        api.liveSessions(),
        api.liveOrder().catch((error: unknown) => {
          if (error instanceof ApiError && error.status === 404)
            return { revision: 0, session_ids: [] };
          throw error;
        }),
      ]);
      const position = new Map(
        order.session_ids.map((id, index) => [id, index]),
      );
      const sorted = sessions.sort((a, b) => {
        const left = position.get(a.id);
        const right = position.get(b.id);
        if (left !== undefined || right !== undefined)
          return (left ?? -1) - (right ?? -1);
        return (
          (b.started_at ?? "").localeCompare(a.started_at ?? "") ||
          a.id.localeCompare(b.id)
        );
      });
      // An in-flight poll that started before a user clicked a tab's ✕ can
      // land after the optimistic removal and put the just-closed session
      // back. Filter those ids here so the tab bar stays feeling immediate.
      return filterClosing(LIVE_SCOPE, sorted);
    },
    // Registration precedes the input socket. Do not leave startup controls
    // disabled for a full status-poll interval after that socket connects.
    refetchInterval: (query) =>
      query.state.data?.some((session) => {
        const age = Date.now() - Date.parse(session.started_at ?? "");
        return session.cli_attached === false && age >= 0 && age < 30_000;
      })
        ? 500
        : REFETCH_INTERVAL_MS,
    staleTime: REFETCH_INTERVAL_MS,
    retry: false,
  });
  const list = enabled ? (query.data ?? []) : [];
  return {
    list,
    activeIds: new Set(list.map((s) => s.id)),
    isLoading: query.isLoading,
  };
}
