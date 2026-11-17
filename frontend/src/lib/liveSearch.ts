import type { LiveSession, Summary } from "./types";
import type { SessionScope } from "./sessionScope";
import { parsePrQuery, prRefMatches } from "./prUrls";

/**
 * Does this live session belong to the active scope tab?
 *
 * Derived from the live session itself (not its stored row) so a freshly
 * registered live session is scoped correctly *immediately* — without waiting
 * for the `["sessions"]` list query to refetch its eager row. A live session is
 * in the poll because the viewer authored it OR it's shared with them, and its
 * eager `sessions` row is always `restricted` until it graduates. So:
 *   mine   — the viewer authored it
 *   shared — it's shared with the viewer (not the author)
 *   org    — never (live rows are restricted, not organization-wide)
 * This matches `matchesScope` applied to the eager row, keeping the LIVE card
 * and its (deduped) stored row in the same scope.
 */
export function liveMatchesScope(
  live: LiveSession,
  scope: SessionScope,
): boolean {
  switch (scope) {
    case "all":
      return true;
    case "mine":
      return live.viewer_is_author === true;
    case "shared":
      return live.viewer_is_author === false;
    case "org":
      return false;
  }
}

/**
 * Does this live session match the active search query + project filter?
 *
 * Mirrors what the server's `list_sessions` matches for stored sessions, scoped
 * to the fields a live session carries: title / project_path substring, and —
 * when the query is a PR reference (URL or `owner/repo#N`) — its linked PRs.
 * An empty query matches everything (so a lone project filter still applies).
 */
export function liveMatchesQuery(
  live: LiveSession,
  query: string,
  project: string,
): boolean {
  // Project filter is an exact path match, same as the stored-session filter.
  if (project && live.project_path !== project) return false;

  const q = query.trim().toLowerCase();
  if (!q) return true;

  if (live.title?.toLowerCase().includes(q)) return true;
  if (live.project_path?.toLowerCase().includes(q)) return true;

  // PR reference: match against the session's linked PRs.
  const ref = parsePrQuery(query);
  if (ref && (live.linked_prs ?? []).some((pr) => prRefMatches(ref, pr))) {
    return true;
  }

  return false;
}

/**
 * Adapt a live session to the `Summary` shape so it can render in the results
 * grid via `SessionCard`. A live (ungraduated) session has no stored row, so the
 * stored-only fields are filled with neutral placeholders; `message_count: 0`
 * is intentionally falsy so the card omits the "N messages" line. The card links
 * to `/sessions/:id`, which renders the live view while the session streams.
 */
export function liveToSummary(live: LiveSession): Summary {
  return {
    id: live.id,
    title: live.title,
    project_path: live.project_path,
    git_branch: null,
    message_count: 0,
    started_at: live.started_at,
    ended_at: null,
    uploaded_by: "",
    uploaded_at: live.started_at ?? "",
    source_host: null,
    owner_email: null,
    // Live sessions are author/sharee-scoped until they graduate.
    visibility: "restricted",
    is_live: true,
    repos: [],
  };
}
