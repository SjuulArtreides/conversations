import type { PrComment } from "./types";

export const prCommentsKey = (id: string) =>
  ["session-pr-comments", id] as const;
export const prCommentPreviewsKey = (id: string) =>
  ["session-pr-comment-previews", id] as const;

export function latestPrCommentRevision(comments: PrComment[]): number {
  return comments.reduce(
    (latest, comment) => Math.max(latest, comment.revision ?? 0),
    0,
  );
}

/** Apply changed comments without replacing the unchanged Markdown inputs. */
export function mergePrComments(
  current: PrComment[],
  changed: PrComment[],
): PrComment[] {
  if (changed.length === 0) return current;
  const byKey = new Map(
    current.map((c) => [`${c.owner}/${c.repo}#${c.number}:${c.comment_id}`, c]),
  );
  for (const comment of changed) {
    const key = `${comment.owner}/${comment.repo}#${comment.number}:${comment.comment_id}`;
    const previous = byKey.get(key);
    if (!previous || (comment.revision ?? 0) >= (previous.revision ?? 0)) {
      byKey.set(key, comment);
    }
  }
  return [...byKey.values()].sort((a, b) =>
    a.created_at.localeCompare(b.created_at),
  );
}
