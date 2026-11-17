import type { PrComment } from "./types";

/**
 * PR-activity comments grouped under the PR they belong to, so the panel can
 * show which repo/PR each thread came from. Each comment already carries its
 * `owner`/`repo`/`number`, so grouping is purely a client-side partition.
 */
export interface PrCommentGroup {
  /** Stable key, `owner/repo#number` lowercased. */
  key: string;
  owner: string;
  repo: string;
  number: number;
  /** Canonical PR URL. */
  url: string;
  /** Open (unresolved) inline review comments, in server order (oldest first). */
  openReviews: PrComment[];
  /** Issue comments + review summaries, in server order. */
  discussion: PrComment[];
  /** Most recent `created_at` among the group's visible comments (RFC3339). */
  latestActivity: string;
}

/** A review comment is "open" (actionable) when its thread is not resolved.
 * `resolved` is absent on legacy rows / non-review kinds — treat unknown as
 * open so we never hide something we're unsure about. */
export function isOpenReviewComment(c: PrComment): boolean {
  return c.kind === "review_comment" && c.resolved !== true;
}

/** A resolved inline review comment — hidden from the panel entirely. */
function isResolvedReviewComment(c: PrComment): boolean {
  return c.kind === "review_comment" && c.resolved === true;
}

/**
 * Group PR comments by their PR, dropping resolved inline review comments.
 *
 * Within a group, comments keep their incoming (server `created_at` ascending)
 * order. Groups are returned most-recently-active first, so the PR with the
 * freshest comment leads. A group with only resolved review comments collapses
 * to nothing and is omitted. `hiddenResolved` (returned separately) lets the
 * caller distinguish "nothing here" from "all resolved 🎉".
 */
export function groupCommentsByPr(comments: PrComment[]): {
  groups: PrCommentGroup[];
  hiddenResolved: number;
} {
  const byKey = new Map<string, PrCommentGroup>();
  let hiddenResolved = 0;

  for (const c of comments) {
    if (isResolvedReviewComment(c)) {
      hiddenResolved += 1;
      continue;
    }
    const key = `${c.owner}/${c.repo}#${c.number}`.toLowerCase();
    let group = byKey.get(key);
    if (!group) {
      group = {
        key,
        owner: c.owner,
        repo: c.repo,
        number: c.number,
        url: `https://github.com/${c.owner}/${c.repo}/pull/${c.number}`,
        openReviews: [],
        discussion: [],
        latestActivity: c.created_at,
      };
      byKey.set(key, group);
    }
    if (isOpenReviewComment(c)) {
      group.openReviews.push(c);
    } else {
      group.discussion.push(c);
    }
    if (c.created_at > group.latestActivity) {
      group.latestActivity = c.created_at;
    }
  }

  const groups = [...byKey.values()].sort((a, b) =>
    a.latestActivity < b.latestActivity ? 1 : a.latestActivity > b.latestActivity ? -1 : 0,
  );
  return { groups, hiddenResolved };
}
