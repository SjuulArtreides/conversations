import { describe, expect, it } from "vitest";
import type { Summary, Visibility } from "./types";
import {
  asScope,
  filterByScope,
  scopeCounts,
  type SessionScope,
} from "./sessionScope";

const ME = "me@x.com";

function s(
  id: string,
  owner_email: string | null,
  visibility: Visibility,
): Summary {
  return {
    id,
    title: id,
    project_path: null,
    git_branch: null,
    message_count: 0,
    started_at: null,
    ended_at: null,
    uploaded_by: owner_email ?? "anon",
    uploaded_at: "2026-06-06T00:00:00.000Z",
    source_host: null,
    owner_email,
    visibility,
    repos: [],
  };
}

// A representative mix: mine (org + restricted), an org session from someone
// else, and a restricted session shared to me by someone else.
const mineOrg = s("mine-org", ME, "organization");
const mineRestricted = s("mine-restricted", ME, "restricted");
const otherOrg = s("other-org", "bob@x.com", "organization");
const sharedToMe = s("shared", "bob@x.com", "restricted");
const ALL = [mineOrg, mineRestricted, otherOrg, sharedToMe];

const ids = (xs: Summary[]) => xs.map((x) => x.id).sort();

describe("asScope", () => {
  it("accepts known scopes and defaults everything else to 'all'", () => {
    expect(asScope("mine")).toBe("mine");
    expect(asScope("shared")).toBe("shared");
    expect(asScope("org")).toBe("org");
    expect(asScope("all")).toBe("all");
    expect(asScope(null)).toBe("all");
    expect(asScope("bogus")).toBe("all");
  });
});

describe("filterByScope", () => {
  it("partitions visible sessions into disjoint, complete scopes", () => {
    const mine = filterByScope(ALL, "mine", ME);
    const shared = filterByScope(ALL, "shared", ME);
    const org = filterByScope(ALL, "org", ME);

    expect(ids(mine)).toEqual(["mine-org", "mine-restricted"]);
    expect(ids(shared)).toEqual(["shared"]);
    expect(ids(org)).toEqual(["other-org"]);

    // Disjoint + complete: the three scopes reconstruct the full list.
    expect(ids([...mine, ...shared, ...org])).toEqual(ids(ALL));
  });

  it("'all' returns everything", () => {
    expect(ids(filterByScope(ALL, "all", ME))).toEqual(ids(ALL));
  });

  it("returns everything for any scope when there is no identity", () => {
    for (const scope of ["all", "mine", "shared", "org"] as SessionScope[]) {
      expect(filterByScope(ALL, scope, null)).toHaveLength(ALL.length);
    }
  });
});

describe("scopeCounts", () => {
  it("counts each disjoint scope plus the total", () => {
    expect(scopeCounts(ALL, ME)).toEqual({
      all: 4,
      mine: 2,
      shared: 1,
      org: 1,
    });
  });
});
