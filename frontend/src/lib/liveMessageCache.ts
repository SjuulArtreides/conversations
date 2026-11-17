import type { Message } from "./types";

/**
 * SPA-lifetime cache of live-session messages, keyed by live-session id and,
 * within a session, by the server-assigned envelope `seq`.
 *
 * Why it exists: switching between live sessions navigates the router (the
 * `:id` param changes), which tears down and re-creates the `useLiveStream`
 * subscription. Without a cache the fresh subscription discards everything and
 * the server replays the *entire* backlog from `seq 0` — so a reader who flips
 * between two sessions re-downloads each full history every time. Holding the
 * per-session `seq → Message` map here lets a re-visit paint instantly from the
 * cache and reconnect with `?since=<maxSeq>`, so the network carries only the
 * tail (see `useLiveStream` + the server's `stream_live` `since` bound).
 *
 * Scope is deliberately **in-memory / SPA lifetime**: a full page reload starts
 * cold (empty cache ⇒ full replay). No localStorage/IndexedDB, no quota surface.
 *
 * `seq` is a dense, gap-free, monotonic 0-based per-session counter (the server
 * assigns `MAX(seq)+1` per append), so the max key a session has cached is an
 * exact, safe resume cursor.
 */

/** Max distinct live sessions kept before the least-recently-used is evicted. */
const MAX_SESSIONS = 24;

// Insertion order doubles as LRU order: `getLiveCache` re-inserts on access so
// the oldest (front) entry is the least-recently-used.
const cache = new Map<string, Map<number, Message>>();

/**
 * The `seq → Message` map for `id`, creating it if absent. The returned map is
 * the live cache entry itself (not a copy): callers mutate it in place and the
 * writes are the cache. Accessing an id marks it most-recently-used; creating a
 * new one past {@link MAX_SESSIONS} evicts the least-recently-used entry.
 */
export function getLiveCache(id: string): Map<number, Message> {
  const existing = cache.get(id);
  if (existing) {
    // Touch: move to the back (most-recently-used).
    cache.delete(id);
    cache.set(id, existing);
    return existing;
  }
  const fresh = new Map<number, Message>();
  cache.set(id, fresh);
  if (cache.size > MAX_SESSIONS) {
    const lru = cache.keys().next().value;
    if (lru !== undefined) cache.delete(lru);
  }
  return fresh;
}

/**
 * Drop a session's cached messages. Called when a session emits its terminal
 * `complete` frame (it graduated — the live id is done, so nothing more will
 * arrive on it and there's no point resuming it).
 */
export function evictLiveCache(id: string): void {
  cache.delete(id);
}

/**
 * The highest `seq` present in `map`, or `undefined` when empty. Used as the
 * `since` resume cursor: `undefined` ⇒ omit `?since` ⇒ full replay.
 */
export function maxSeq(map: Map<number, unknown>): number | undefined {
  let max: number | undefined;
  for (const seq of map.keys()) {
    if (max === undefined || seq > max) max = seq;
  }
  return max;
}

/** Test-only: empty the message cache so cases don't leak state. */
export function __resetLiveCacheForTest(): void {
  cache.clear();
}
