import type { Summary } from "./types";

/**
 * Which slice of the visible sessions to show. The three identity-relative
 * scopes partition everything the viewer can see into disjoint sets:
 *   mine   — sessions I own (any visibility)
 *   shared — restricted sessions someone explicitly shared with me
 *   org    — organization-wide sessions owned by someone else
 * `all` is the union (the server already scopes the list to what I may read).
 */
export type SessionScope = "all" | "mine" | "shared" | "org";

export const SESSION_SCOPES: readonly { id: SessionScope; label: string }[] = [
  { id: "all", label: "All" },
  { id: "mine", label: "My sessions" },
  { id: "shared", label: "Shared with me" },
  { id: "org", label: "Organization" },
] as const;

const VALID = new Set<string>(SESSION_SCOPES.map((s) => s.id));

/** Coerce an untrusted value (e.g. a URL param) to a scope, defaulting to `all`. */
export function asScope(value: string | null | undefined): SessionScope {
  return value && VALID.has(value) ? (value as SessionScope) : "all";
}

/** Does this session belong to `scope` from `myEmail`'s perspective? */
export function matchesScope(
  s: Summary,
  scope: SessionScope,
  myEmail: string,
): boolean {
  const mine = s.owner_email === myEmail;
  switch (scope) {
    case "all":
      return true;
    case "mine":
      return mine;
    case "shared":
      return !mine && s.visibility === "restricted";
    case "org":
      return !mine && s.visibility === "organization";
  }
}

/**
 * Filter `sessions` to `scope`. Without an identity (`myEmail` null — auth off)
 * the identity-relative scopes are meaningless, so everything is returned.
 */
export function filterByScope(
  sessions: Summary[],
  scope: SessionScope,
  myEmail: string | null,
): Summary[] {
  if (scope === "all" || !myEmail) return sessions;
  return sessions.filter((s) => matchesScope(s, scope, myEmail));
}

/** Count how many of `sessions` fall into each scope (for tab badges). */
export function scopeCounts(
  sessions: Summary[],
  myEmail: string,
): Record<SessionScope, number> {
  const counts: Record<SessionScope, number> = {
    all: sessions.length,
    mine: 0,
    shared: 0,
    org: 0,
  };
  for (const s of sessions) {
    if (matchesScope(s, "mine", myEmail)) counts.mine++;
    else if (matchesScope(s, "shared", myEmail)) counts.shared++;
    else if (matchesScope(s, "org", myEmail)) counts.org++;
  }
  return counts;
}
