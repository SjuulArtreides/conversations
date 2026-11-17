import { describe, expect, it } from "vitest";
import { extractPrUrls, parsePrQuery, prAnchorId, prRefMatches } from "./prUrls";

describe("prAnchorId", () => {
  it("builds a stable, DOM-safe id from owner/repo/number", () => {
    expect(prAnchorId("acme", "my-repo", 42)).toBe("pr-anchor-acme-my-repo-42");
  });

  it("is case-insensitive and replaces non-id chars (so URL casing doesn't matter)", () => {
    expect(prAnchorId("Example-Org", "example.platform", 7)).toBe(
      prAnchorId("example-org", "example-platform", 7),
    );
    expect(prAnchorId("A.b_c", "x", 1)).toBe("pr-anchor-a-b-c-x-1");
  });
});

describe("extractPrUrls", () => {
  it("extracts a single valid PR URL", () => {
    const text = "Check out https://github.com/acme/my-repo/pull/42 for the fix.";
    expect(extractPrUrls(text)).toEqual([
      { url: "https://github.com/acme/my-repo/pull/42", owner: "acme", repo: "my-repo", number: 42 },
    ]);
  });

  it("extracts multiple distinct PR URLs from the same text", () => {
    const text =
      "PR https://github.com/org/alpha/pull/1 and https://github.com/org/beta/pull/99 are both merged.";
    expect(extractPrUrls(text)).toEqual([
      { url: "https://github.com/org/alpha/pull/1", owner: "org", repo: "alpha", number: 1 },
      { url: "https://github.com/org/beta/pull/99", owner: "org", repo: "beta", number: 99 },
    ]);
  });

  it("de-dupes the same URL appearing twice", () => {
    const text =
      "https://github.com/acme/repo/pull/7 and https://github.com/acme/repo/pull/7 again";
    const results = extractPrUrls(text);
    expect(results).toHaveLength(1);
    expect(results[0].number).toBe(7);
  });

  it("ignores trailing /files path segment — stops at the PR number", () => {
    const text = "See https://github.com/acme/repo/pull/10/files for the diff.";
    const results = extractPrUrls(text);
    // The URL captured is the canonical one without /files.
    expect(results).toEqual([
      { url: "https://github.com/acme/repo/pull/10", owner: "acme", repo: "repo", number: 10 },
    ]);
  });

  it("ignores query strings after the number", () => {
    const text = "https://github.com/acme/repo/pull/5?diff=unified";
    const results = extractPrUrls(text);
    expect(results).toEqual([
      { url: "https://github.com/acme/repo/pull/5", owner: "acme", repo: "repo", number: 5 },
    ]);
  });

  it("returns empty array when text contains no PR URLs", () => {
    expect(extractPrUrls("No links here at all.")).toEqual([]);
  });

  it("returns empty array for non-PR GitHub URLs", () => {
    const text =
      "https://github.com/acme/repo/issues/3 and https://github.com/acme/repo/commit/abc123";
    expect(extractPrUrls(text)).toEqual([]);
  });

  it("handles repos with dots and underscores in their name", () => {
    const text = "https://github.com/my-org/my.repo_v2/pull/100";
    expect(extractPrUrls(text)).toEqual([
      { url: "https://github.com/my-org/my.repo_v2/pull/100", owner: "my-org", repo: "my.repo_v2", number: 100 },
    ]);
  });

  it("returns empty array for empty string", () => {
    expect(extractPrUrls("")).toEqual([]);
  });
});

describe("parsePrQuery", () => {
  it("parses a full PR URL", () => {
    expect(
      parsePrQuery("https://github.com/Example-Org/product-demo/pull/409"),
    ).toEqual({
      url: "https://github.com/Example-Org/product-demo/pull/409",
      owner: "Example-Org",
      repo: "product-demo",
      number: 409,
    });
  });

  it("parses a scheme-less URL and ignores trailing path", () => {
    expect(parsePrQuery("github.com/acme/repo/pull/12/files")).toMatchObject({
      owner: "acme",
      repo: "repo",
      number: 12,
    });
  });

  it("parses the owner/repo#N shorthand (with surrounding whitespace)", () => {
    expect(parsePrQuery("  acme/widgets#7  ")).toMatchObject({
      owner: "acme",
      repo: "widgets",
      number: 7,
    });
  });

  it("returns null for non-PR queries", () => {
    expect(parsePrQuery("review pull request")).toBeNull();
    expect(parsePrQuery("#409")).toBeNull();
    expect(parsePrQuery("acme/widgets#0")).toBeNull();
    expect(parsePrQuery("just text")).toBeNull();
  });
});

describe("prRefMatches", () => {
  const ref = {
    url: "https://github.com/Example-Org/product-demo/pull/409",
    owner: "Example-Org",
    repo: "product-demo",
    number: 409,
  };

  it("matches case-insensitively on owner/repo + exact number", () => {
    expect(
      prRefMatches(ref, {
        owner: "example-org",
        repo: "product-demo",
        number: 409,
      }),
    ).toBe(true);
  });

  it("rejects a different number or repo", () => {
    expect(
      prRefMatches(ref, { owner: "Example-Org", repo: "product-demo", number: 410 }),
    ).toBe(false);
    expect(
      prRefMatches(ref, { owner: "Example-Org", repo: "other", number: 409 }),
    ).toBe(false);
  });
});
