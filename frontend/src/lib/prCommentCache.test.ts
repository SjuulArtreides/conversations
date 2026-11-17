import { describe, expect, it } from "vitest";
import { latestPrCommentRevision, mergePrComments } from "./prCommentCache";
import type { PrComment } from "./types";

const comment = (
  comment_id: string,
  body: string,
  created_at: string,
): PrComment => ({
  owner: "org",
  repo: "repo",
  number: 1,
  comment_id,
  kind: "issue",
  author: "reviewer",
  body,
  html_url: `https://github.com/org/repo/pull/1#${comment_id}`,
  created_at,
  ingested_at: created_at,
});

describe("PR comment deltas", () => {
  it("updates an edited comment without duplicating it or replacing unrelated comments", () => {
    const first = comment("1", "original", "2026-10-19T10:00:00Z");
    const second = comment("2", "unchanged", "2026-10-19T11:00:00Z");
    const next = mergePrComments(
      [first, second],
      [
        { ...first, body: "edited" },
        comment("3", "new", "2026-10-19T12:00:00Z"),
      ],
    );
    expect(next.map((item) => item.comment_id)).toEqual(["1", "2", "3"]);
    expect(next[0].body).toBe("edited");
    expect(next[1]).toBe(second);
    expect(mergePrComments(next, [])).toBe(next);
  });

  it("does not roll an edited comment back when delta requests complete out of order", () => {
    const current = {
      ...comment("1", "newer", "2026-10-19T10:00:00Z"),
      revision: 7,
    };
    const stale = { ...current, body: "older", revision: 6 };
    expect(mergePrComments([current], [stale])[0]).toBe(current);
    expect(latestPrCommentRevision([stale, current])).toBe(7);
  });
});
