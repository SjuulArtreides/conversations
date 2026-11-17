import { beforeEach, describe, expect, it } from "vitest";
import {
  __resetLiveCacheForTest,
  evictLiveCache,
  getLiveCache,
  maxSeq,
} from "./liveMessageCache";
import type { Message } from "./types";

function msg(uuid: string): Message {
  return {
    uuid,
    role: "assistant",
    text: uuid,
    blocks: [],
  } as unknown as Message;
}

beforeEach(() => {
  __resetLiveCacheForTest();
});

describe("getLiveCache", () => {
  it("returns the same map reference across calls for one id (writes persist)", () => {
    const a = getLiveCache("s1");
    a.set(0, msg("m0"));
    const again = getLiveCache("s1");
    expect(again).toBe(a);
    expect(again.get(0)?.uuid).toBe("m0");
  });

  it("keeps separate maps per session id", () => {
    getLiveCache("s1").set(0, msg("a"));
    getLiveCache("s2").set(0, msg("b"));
    expect(getLiveCache("s1").get(0)?.uuid).toBe("a");
    expect(getLiveCache("s2").get(0)?.uuid).toBe("b");
  });

  it("evicts the least-recently-used session past the cap", () => {
    // Fill past the 24-session cap; the first-inserted, never-touched id goes.
    for (let i = 0; i < 25; i++) getLiveCache(`s${i}`).set(0, msg(`m${i}`));
    // s0 was inserted first and never re-accessed ⇒ evicted (fresh, empty map).
    expect(getLiveCache("s0").size).toBe(0);
    // s24 (most recent) still holds its message.
    expect(getLiveCache("s24").get(0)?.uuid).toBe("m24");
  });

  it("touch-on-access spares a recently-used session from eviction", () => {
    getLiveCache("keep").set(0, msg("keep0"));
    // Touch "keep" so it's most-recently-used, then overflow the cap.
    for (let i = 0; i < 24; i++) {
      getLiveCache("keep"); // touch
      getLiveCache(`f${i}`).set(0, msg(`f${i}`));
    }
    expect(getLiveCache("keep").get(0)?.uuid).toBe("keep0");
  });
});

describe("evictLiveCache", () => {
  it("drops a session so a re-fetch starts empty", () => {
    getLiveCache("s1").set(0, msg("m0"));
    evictLiveCache("s1");
    expect(getLiveCache("s1").size).toBe(0);
  });
});

describe("maxSeq", () => {
  it("is undefined for an empty map (⇒ full replay)", () => {
    expect(maxSeq(new Map())).toBeUndefined();
  });

  it("returns the highest seq regardless of insertion order", () => {
    const m = new Map<number, Message>();
    m.set(2, msg("c"));
    m.set(0, msg("a"));
    m.set(1, msg("b"));
    expect(maxSeq(m)).toBe(2);
  });
});
