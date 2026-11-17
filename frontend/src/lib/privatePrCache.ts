import { hostControl } from "./hostControl";
import { toBase64Url } from "./hostAuth";
import type { SessionPr, PrComment } from "./types";
export interface PrCache {
  prs: SessionPr[];
  comments: PrComment[];
}
const parsed = new Map<string, { sha: string; value: PrCache }>();
const inFlight = new Map<string, Promise<PrCache>>();
const changed = "PR cache changed; restart from offset 0";

function validate(value: unknown): PrCache {
  if (!value || typeof value !== "object")
    throw new Error("Unreadable PR cache.");
  const cache = value as PrCache;
  if (!Array.isArray(cache.prs) || !Array.isArray(cache.comments))
    throw new Error("Unreadable PR cache.");
  for (const pr of cache.prs) {
    if (
      ![pr.url, pr.owner, pr.repo].every((v) => typeof v === "string") ||
      !Number.isSafeInteger(pr.number) ||
      pr.number < 1
    )
      throw new Error("Unreadable pull request.");
    if (
      pr.status !== null &&
      (!pr.status ||
        !["OPEN", "CLOSED", "MERGED"].includes(pr.status.state) ||
        typeof pr.status.title !== "string" ||
        typeof pr.status.merged !== "boolean" ||
        typeof pr.status.draft !== "boolean")
    )
      throw new Error("Unreadable pull request status.");
  }
  for (const comment of cache.comments) {
    if (
      ![
        comment.owner,
        comment.repo,
        comment.comment_id,
        comment.kind,
        comment.author,
        comment.body,
        comment.html_url,
        comment.created_at,
        comment.ingested_at,
      ].every((v) => typeof v === "string") ||
      !Number.isSafeInteger(comment.number) ||
      comment.number < 1
    )
      throw new Error("Unreadable PR comment.");
  }
  return cache;
}
/** Assemble and verify a complete cached snapshot. Reading never asks GitHub to sync. */
export function readPrivatePrCache(
  hostId: string,
  session: string,
): Promise<PrCache> {
  const key = `${hostId}:${session}`;
  const pending = inFlight.get(key);
  if (pending) return pending;
  const promise = (async () => {
    for (let attempt = 0; attempt < 2; attempt++) {
      let offset = 0,
        sha: string | undefined;
      const chunks: Uint8Array[] = [];
      while (true) {
        const reply = await hostControl.privateRead(hostId, (channel) =>
          channel.operation("pr.cache.list", session, {
            session,
            offset,
            ...(sha ? { sha256: sha } : {}),
          }),
        );
        if (reply.status === "refused" && reply.detail === changed) {
          break;
        }
        if (reply.status !== "executed" || reply.body?.kind !== "pr_cache")
          throw new Error(
            reply.detail ?? "The host could not read its PR cache.",
          );
        const body = reply.body;
        if (sha && body.sha256 !== sha) {
          break;
        }
        sha = body.sha256;
        const cached = parsed.get(key);
        if (offset === 0 && cached?.sha === sha) return cached.value;
        const bytes = Uint8Array.from(atob(body.b64), (c) => c.charCodeAt(0));
        chunks.push(bytes);
        if (body.next === null) {
          const all = new Uint8Array(offset + bytes.length);
          let cursor = 0;
          for (const chunk of chunks) {
            all.set(chunk, cursor);
            cursor += chunk.length;
          }
          if (
            toBase64Url(
              new Uint8Array(await crypto.subtle.digest("SHA-256", all)),
            ) !== sha
          )
            throw new Error(
              "The host's PR cache digest did not match. Retry the PR list.",
            );
          const value = validate(
            JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(all)),
          );
          parsed.set(key, { sha, value });
          if (parsed.size > 64) parsed.delete(parsed.keys().next().value!);
          return value;
        }
        if (body.next !== offset + bytes.length || body.next <= offset)
          throw new Error("The host's PR cache cursor did not advance.");
        offset = body.next;
      }
    }
    throw new Error("The host's PR cache kept changing. Retry the PR list.");
  })().finally(() => inFlight.delete(key));
  inFlight.set(key, promise);
  return promise;
}
