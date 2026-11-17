// Scopes for pending-close: "live-sessions" is the shared/remote tab bar; a
// private scope is `private:${hostId}` so one host's optimistic close never
// masks another host's running session in a shared cache.
const pending = new Map<string, Set<string>>();

/** Mark `id` as closing in `scope` until the matching clearClosing call. */
export function markClosing(scope: string, id: string): void {
  let set = pending.get(scope);
  if (!set) {
    set = new Set();
    pending.set(scope, set);
  }
  set.add(id);
}

/** Drop `id` from `scope`; empties the per-scope set when the last id leaves. */
export function clearClosing(scope: string, id: string): void {
  const set = pending.get(scope);
  if (!set) return;
  set.delete(id);
  if (set.size === 0) pending.delete(scope);
}

/**
 * Filter entries whose id is still marked as closing in `scope`. Call from a
 * query's queryFn so an older in-flight fetch that lands after close started
 * cannot restore the tab the user just dismissed.
 */
export function filterClosing<T extends { id: string }>(
  scope: string,
  list: T[],
): T[] {
  const set = pending.get(scope);
  return set && set.size > 0 ? list.filter((item) => !set!.has(item.id)) : list;
}

/** Scope name for a paired host's private-session caches. */
export function privateScope(hostId: string): string {
  return `private:${hostId}`;
}

/** Scope name for the shared/remote live-session tab bar. */
export const LIVE_SCOPE = "live-sessions";
