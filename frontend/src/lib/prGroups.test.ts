import { describe, expect, it } from "vitest";
import { groupCommentsByPr } from "./prGroups";
import type { PrComment } from "./types";

function comment(overrides: Partial<PrComment> = {}): PrComment {
  return {
    owner: "acme",
    repo: "widgets",
    number: 42,
    comment_id: "c1",
    kind: "issue",
    author: "bob",
    body: "hi",
    html_url: "https://github.com/acme/widgets/pull/42#c1",
    created_at: "2026-06-07T10:00:00Z",
    ingested_at: "2026-06-07T10:00:05Z",
    ...overrides,
  };
}

describe("groupCommentsByPr", () => {
  it("groups comments under their PR with a canonical URL", () => {
    const { groups } = groupCommentsByPr([
      comment({ comment_id: "a", number: 42 }),
      comment({ comment_id: "b", number: 42, repo: "widgets" }),
    ]);
    expect(groups).toHaveLength(1);
    expect(groups[0].key).toBe("acme/widgets#42");
    expect(groups[0].url).toBe("https://github.com/acme/widgets/pull/42");
    expect(groups[0].discussion).toHaveLength(2);
  });

  it("separates distinct PRs and orders most-recently-active first", () => {
    const { groups } = groupCommentsByPr([
      comment({ comment_id: "old", number: 1, created_at: "2026-06-01T00:00:00Z" }),
      comment({ comment_id: "new", number: 2, created_at: "2026-06-09T00:00:00Z" }),
    ]);
    expect(groups.map((g) => g.number)).toEqual([2, 1]);
  });

  it("splits open review comments from discussion within a group", () => {
    const { groups } = groupCommentsByPr([
      comment({ comment_id: "issue", kind: "issue" }),
      comment({
        comment_id: "open",
        kind: "review_comment",
        resolved: false,
        path: "src/main.rs",
        line: 4,
      }),
    ]);
    expect(groups).toHaveLength(1);
    expect(groups[0].openReviews.map((c) => c.comment_id)).toEqual(["open"]);
    expect(groups[0].discussion.map((c) => c.comment_id)).toEqual(["issue"]);
  });

  it("drops resolved review comments and counts them", () => {
    const { groups, hiddenResolved } = groupCommentsByPr([
      comment({ comment_id: "done", kind: "review_comment", resolved: true }),
    ]);
    expect(groups).toHaveLength(0);
    expect(hiddenResolved).toBe(1);
  });

  it("groups case-insensitively on owner/repo", () => {
    const { groups } = groupCommentsByPr([
      comment({ comment_id: "a", owner: "Acme", repo: "Widgets", number: 5 }),
      comment({ comment_id: "b", owner: "acme", repo: "widgets", number: 5 }),
    ]);
    expect(groups).toHaveLength(1);
    expect(groups[0].discussion).toHaveLength(2);
  });
});
