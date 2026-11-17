import "fake-indexeddb/auto";
import { afterEach, describe, expect, it } from "vitest";
import {
  clearTerminalCache,
  deleteTerminalSnapshot,
  enableTerminalCache,
  loadTerminalSnapshot,
  saveTerminalSnapshot,
  suspendTerminalCache,
} from "./terminalCache";

afterEach(async () => {
  enableTerminalCache();
  await clearTerminalCache();
});

describe("terminalCache", () => {
  it("restores a saved terminal framebuffer", async () => {
    await saveTerminalSnapshot("session-1", "cached terminal", 120, 40);

    await expect(loadTerminalSnapshot("session-1")).resolves.toMatchObject({
      serialized: "cached terminal",
      cols: 120,
      rows: 40,
    });
  });

  it("keeps the eight most recently used sessions", async () => {
    for (let index = 1; index <= 8; index++) {
      await saveTerminalSnapshot(
        `session-${index}`,
        `terminal ${index}`,
        80,
        24,
      );
    }

    // Touch the oldest entry. The next insert must evict session-2 instead.
    expect(await loadTerminalSnapshot("session-1")).not.toBeNull();
    await saveTerminalSnapshot("session-9", "terminal 9", 80, 24);

    expect(await loadTerminalSnapshot("session-1")).not.toBeNull();
    expect(await loadTerminalSnapshot("session-2")).toBeNull();
    expect(await loadTerminalSnapshot("session-9")).not.toBeNull();
  });

  it("removes a completed terminal snapshot", async () => {
    await saveTerminalSnapshot("session-1", "cached terminal", 80, 24);
    await deleteTerminalSnapshot("session-1");

    await expect(loadTerminalSnapshot("session-1")).resolves.toBeNull();
  });

  it("removes a snapshot with invalid terminal dimensions", async () => {
    await saveTerminalSnapshot("session-1", "cached terminal", 80, 24);
    await saveTerminalSnapshot("session-1", "invalid terminal", 0, 24);

    await expect(loadTerminalSnapshot("session-1")).resolves.toBeNull();
  });

  it("does not let an unmount write repopulate the cache after sign-out", async () => {
    await saveTerminalSnapshot("session-1", "cached terminal", 80, 24);
    await suspendTerminalCache();

    await saveTerminalSnapshot("session-1", "late unmount write", 80, 24);

    await expect(loadTerminalSnapshot("session-1")).resolves.toBeNull();
  });

  it("does not let an in-flight recency update repopulate the cache after sign-out", async () => {
    await saveTerminalSnapshot("session-1", "cached terminal", 80, 24);

    const load = loadTerminalSnapshot("session-1");
    const suspend = suspendTerminalCache();
    await Promise.all([load, suspend]);
    enableTerminalCache();

    await expect(loadTerminalSnapshot("session-1")).resolves.toBeNull();
  });
});
