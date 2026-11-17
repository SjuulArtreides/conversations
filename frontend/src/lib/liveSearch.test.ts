import { describe, expect, it } from "vitest";
import { liveMatchesQuery, liveMatchesScope, liveToSummary } from "./liveSearch";
import type { LiveSession } from "./types";

function live(overrides: Partial<LiveSession> = {}): LiveSession {
  return {
    id: "live-1",
    session_id: "live-1",
    title: "Review pull request #409",
    project_path: "/Users/me/product-demo",
    started_at: "2026-06-09T10:00:00Z",
    last_event_at: "2026-06-09T10:01:00Z",
    mode: "view",
    viewer_is_author: true,
    viewer_can_write: true,
    linked_prs: [
      {
        url: "https://github.com/Example-Org/product-demo/pull/409",
        owner: "Example-Org",
        repo: "product-demo",
        number: 409,
        status: null,
      },
    ],
    ...overrides,
  };
}

describe("liveMatchesQuery", () => {
  it("matches a pasted PR URL against a linked PR", () => {
    expect(
      liveMatchesQuery(
        live(),
        "https://github.com/Example-Org/product-demo/pull/409",
        "",
      ),
    ).toBe(true);
  });

  it("matches the owner/repo#N shorthand case-insensitively", () => {
    expect(liveMatchesQuery(live(), "example-org/product-demo#409", "")).toBe(
      true,
    );
  });

  it("matches a plain substring of the title", () => {
    expect(liveMatchesQuery(live(), "pull request", "")).toBe(true);
  });

  it("does not match a different PR number", () => {
    expect(
      liveMatchesQuery(
        live(),
        "https://github.com/Example-Org/product-demo/pull/410",
        "",
      ),
    ).toBe(false);
  });

  it("does not match a PR when there are no linked PRs", () => {
    expect(
      liveMatchesQuery(
        live({ linked_prs: [] }),
        "https://github.com/Example-Org/product-demo/pull/409",
        "",
      ),
    ).toBe(false);
  });

  it("applies the exact project filter", () => {
    expect(liveMatchesQuery(live(), "", "/Users/me/product-demo")).toBe(true);
    expect(liveMatchesQuery(live(), "", "/some/other/path")).toBe(false);
  });

  it("matches everything on an empty query", () => {
    expect(liveMatchesQuery(live(), "", "")).toBe(true);
  });
});

describe("liveMatchesScope", () => {
  it("shows everything on the 'all' scope", () => {
    expect(liveMatchesScope(live({ viewer_is_author: true }), "all")).toBe(true);
    expect(liveMatchesScope(live({ viewer_is_author: false }), "all")).toBe(
      true,
    );
  });

  it("'mine' includes only sessions the viewer authored", () => {
    expect(liveMatchesScope(live({ viewer_is_author: true }), "mine")).toBe(
      true,
    );
    expect(liveMatchesScope(live({ viewer_is_author: false }), "mine")).toBe(
      false,
    );
  });

  it("'shared' includes only sessions shared with the viewer", () => {
    expect(liveMatchesScope(live({ viewer_is_author: false }), "shared")).toBe(
      true,
    );
    expect(liveMatchesScope(live({ viewer_is_author: true }), "shared")).toBe(
      false,
    );
  });

  it("'org' never includes a live session (live rows are restricted)", () => {
    expect(liveMatchesScope(live({ viewer_is_author: true }), "org")).toBe(
      false,
    );
    expect(liveMatchesScope(live({ viewer_is_author: false }), "org")).toBe(
      false,
    );
  });
});

describe("liveToSummary", () => {
  it("adapts a live session to a Summary with a falsy message count", () => {
    const s = liveToSummary(live());
    expect(s.id).toBe("live-1");
    expect(s.title).toBe("Review pull request #409");
    expect(s.message_count).toBe(0);
    expect(s.repos).toEqual([]);
    expect(s.visibility).toBe("restricted");
  });
});
