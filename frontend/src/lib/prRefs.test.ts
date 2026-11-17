import { describe, expect, it } from "vitest";
import {
  deriveRepoContext,
  deriveRepoContextFromTexts,
  findPrRefs,
} from "./prRefs";

const CTX = { owner: "Example-Org", repo: "example-platform" };

describe("findPrRefs — full URLs", () => {
  it("finds a full PR URL with its span", () => {
    const text = "Check https://github.com/acme/repo/pull/42 now";
    const refs = findPrRefs(text);
    expect(refs).toHaveLength(1);
    expect(refs[0].raw).toBe("https://github.com/acme/repo/pull/42");
    expect(refs[0].ref).toEqual({
      url: "https://github.com/acme/repo/pull/42",
      owner: "acme",
      repo: "repo",
      number: 42,
    });
    expect(text.slice(refs[0].start, refs[0].end)).toBe(refs[0].raw);
  });

  it("finds full URLs without a repo context", () => {
    const refs = findPrRefs("https://github.com/o/r/pull/1");
    expect(refs).toHaveLength(1);
    expect(refs[0].ref.number).toBe(1);
  });
});

describe("findPrRefs — short forms (need repo context)", () => {
  it("does NOT match short forms without a repo context", () => {
    expect(findPrRefs("we finished PR #193 today")).toEqual([]);
    expect(findPrRefs("Get PR 193 review comments")).toEqual([]);
  });

  it("matches `PR #193` against the repo context", () => {
    const text = "We wrapped up the PR #193 work";
    const refs = findPrRefs(text, CTX);
    expect(refs).toHaveLength(1);
    expect(refs[0].raw).toBe("PR #193");
    expect(refs[0].ref.url).toBe(
      "https://github.com/Example-Org/example-platform/pull/193",
    );
    expect(refs[0].ref.number).toBe(193);
  });

  it("matches the no-`#` form `PR 193` (tool label style)", () => {
    const refs = findPrRefs("Get PR 193 review comments", CTX);
    expect(refs).toHaveLength(1);
    expect(refs[0].raw).toBe("PR 193");
    expect(refs[0].ref.number).toBe(193);
  });

  it("matches `pull request #6` case-insensitively", () => {
    const refs = findPrRefs("see Pull Request #6 please", CTX);
    expect(refs).toHaveLength(1);
    expect(refs[0].ref.number).toBe(6);
  });

  it("does not match a keyword embedded in a word", () => {
    // `compr 5` — the `pr` is not on a word boundary.
    expect(findPrRefs("compr 5 things", CTX)).toEqual([]);
  });
});

describe("findPrRefs — ordering, overlap, multiple", () => {
  it("returns matches ordered by position", () => {
    const text = "first PR #2 then https://github.com/o/r/pull/9";
    const refs = findPrRefs(text, { owner: "o", repo: "r" });
    expect(refs.map((r) => r.ref.number)).toEqual([2, 9]);
    expect(refs[0].start).toBeLessThan(refs[1].start);
  });

  it("prefers the full URL over a short ref nested in the same span", () => {
    // The trailing `/193` could be read as a number, but the URL match wins
    // and the short-form scan won't double-cover the same characters.
    const text = "https://github.com/o/r/pull/193";
    const refs = findPrRefs(text, { owner: "o", repo: "r" });
    expect(refs).toHaveLength(1);
    expect(refs[0].raw).toBe("https://github.com/o/r/pull/193");
  });

  it("keeps each occurrence (does not de-dupe by URL — spans matter)", () => {
    const refs = findPrRefs("PR #1 and PR #1 again", { owner: "o", repo: "r" });
    expect(refs).toHaveLength(2);
  });
});

describe("deriveRepoContext", () => {
  it("returns the single repo when all URLs agree", () => {
    expect(
      deriveRepoContext([
        { owner: "o", repo: "r" },
        { owner: "o", repo: "r" },
      ]),
    ).toEqual({ owner: "o", repo: "r" });
  });

  it("returns null on conflicting repos", () => {
    expect(
      deriveRepoContext([
        { owner: "o", repo: "r" },
        { owner: "o", repo: "other" },
      ]),
    ).toBeNull();
  });

  it("returns null on an empty list", () => {
    expect(deriveRepoContext([])).toBeNull();
  });

  it("deriveRepoContextFromTexts pulls the repo from full URLs in the transcript", () => {
    const ctx = deriveRepoContextFromTexts([
      "see https://github.com/Example-Org/example-platform/pull/193",
      "Get PR 193 review comments", // short form contributes nothing
    ]);
    expect(ctx).toEqual(CTX);
  });
});
