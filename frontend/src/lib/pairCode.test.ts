import { beforeEach, describe, expect, it, vi } from "vitest";
import { pairCode, WORDS } from "./pairCode";

describe("pairCode", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("derives the same words from either side (order-independent)", async () => {
    const owner = "chd1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const colleague = "chd1.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    // The owner derives from (own key, vouch key); the colleague from
    // (own key, ceremony host key) — the same two keys in a different order.
    const fromOwner = await pairCode(owner, colleague);
    const fromColleague = await pairCode(colleague, owner);
    expect(fromOwner).toBe(fromColleague);
    expect(fromOwner.split(" ")).toHaveLength(6);
    // Distinct-enough words from the fixed list, lowercase.
    for (const word of fromOwner.split(" ")) {
      expect(WORDS).toContain(word);
    }
  });

  it("differs for different key pairs", async () => {
    const a = await pairCode("chd1.A", "chd1.B");
    const b = await pairCode("chd1.A", "chd1.C");
    expect(a).not.toBe(b);
  });

  it("has a 256-word list of distinct words", () => {
    expect(WORDS.length).toBe(256);
    expect(new Set(WORDS).size).toBe(256);
  });
});
