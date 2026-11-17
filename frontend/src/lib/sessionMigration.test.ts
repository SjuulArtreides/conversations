import { beforeEach, expect, it, vi } from "vitest";
import { toBase64Url } from "./hostAuth";
import { hostControl } from "./hostControl";
import {
  migrateSession,
  MigrationResumeError,
  MigrationCopyUncertainError,
} from "./sessionMigration";

vi.mock("./hostControl", () => ({
  hostControl: {
    exportSessionChunk: vi.fn(),
    importSessionChunk: vi.fn(),
    reopenSession: vi.fn(),
    endSession: vi.fn(),
  },
}));
beforeEach(() => vi.resetAllMocks());
const manifest = {
  agent: "claude" as const,
  native_id: "native",
  title: "Original",
  size: 6,
  sha256: "digest",
};
const args = {
  sourceHost: "laptop",
  sourceSession: "source",
  destinationHost: "workstation",
  cwd: "/project",
};
function chunks() {
  vi.mocked(hostControl.exportSessionChunk)
    .mockResolvedValueOnce({
      kind: "migration",
      manifest,
      b64: toBase64Url(new TextEncoder().encode("abc")),
      next: 3,
    })
    .mockResolvedValueOnce({
      kind: "migration",
      manifest,
      b64: toBase64Url(new TextEncoder().encode("def")),
      next: null,
    });
  vi.mocked(hostControl.importSessionChunk)
    .mockResolvedValueOnce(false)
    .mockResolvedValueOnce(true);
  vi.mocked(hostControl.reopenSession).mockResolvedValue({ delivered: true });
}
it("copies each verified chunk before resuming the destination and keeps the source", async () => {
  chunks();
  const progress = vi.fn();
  const id = await migrateSession({ ...args, onProgress: progress });
  const writes = vi.mocked(hostControl.importSessionChunk).mock.calls;
  expect(
    writes.map(([host, chunk]) => [host, chunk.destination, chunk.offset]),
  ).toEqual([
    ["workstation", id, 0],
    ["workstation", id, 3],
  ]);
  expect(writes[0][1].transfer).toBe(writes[1][1].transfer);
  expect(writes[0][1].destination).not.toBe("source");
  expect(hostControl.reopenSession).toHaveBeenCalledWith(id, "workstation");
  expect(
    vi.mocked(hostControl.reopenSession).mock.invocationCallOrder[0],
  ).toBeGreaterThan(
    vi.mocked(hostControl.importSessionChunk).mock.invocationCallOrder[1],
  );
  expect(hostControl.endSession).not.toHaveBeenCalled();
  expect(progress.mock.calls).toEqual([
    [3, 6],
    [6, 6],
  ]);
});
it("never resumes an incomplete or changed snapshot", async () => {
  chunks();
  vi.mocked(hostControl.exportSessionChunk)
    .mockReset()
    .mockResolvedValueOnce({
      kind: "migration",
      manifest,
      b64: toBase64Url(new Uint8Array([1, 2, 3])),
      next: 3,
    })
    .mockResolvedValueOnce({
      kind: "migration",
      manifest: { ...manifest, sha256: "changed" },
      b64: toBase64Url(new Uint8Array([4, 5, 6])),
      next: null,
    });
  await expect(migrateSession(args)).rejects.toThrow("snapshot changed");
  expect(hostControl.importSessionChunk).toHaveBeenCalledTimes(1);
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
});
it("rejects an early end before sending it to the destination", async () => {
  vi.mocked(hostControl.exportSessionChunk).mockResolvedValue({
    kind: "migration",
    manifest,
    b64: toBase64Url(new Uint8Array([1, 2, 3])),
    next: null,
  });
  await expect(migrateSession(args)).rejects.toThrow("incomplete");
  expect(hostControl.importSessionChunk).not.toHaveBeenCalled();
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
});
it("retains the copied identity when destination startup fails, without copying again", async () => {
  chunks();
  vi.mocked(hostControl.reopenSession).mockRejectedValue(
    new Error("agent unavailable"),
  );
  const error = await migrateSession(args).catch((error) => error);
  expect(error).toBeInstanceOf(MigrationResumeError);
  expect(error.destination).toBe(
    vi.mocked(hostControl.importSessionChunk).mock.calls[0][1].destination,
  );
  expect(error.message).toContain("was copied");
  expect(hostControl.endSession).not.toHaveBeenCalled();
  expect(hostControl.reopenSession).toHaveBeenCalledTimes(1);
});

it("reports an uncertain final receipt without retrying or resuming", async () => {
  chunks();
  vi.mocked(hostControl.importSessionChunk)
    .mockReset()
    .mockResolvedValueOnce(false)
    .mockRejectedValueOnce(new Error("connection lost"));
  const error = await migrateSession(args).catch((error) => error);
  expect(error).toBeInstanceOf(MigrationCopyUncertainError);
  expect(error.destination).toBe(
    vi.mocked(hostControl.importSessionChunk).mock.calls[1][1].destination,
  );
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
  expect(hostControl.endSession).not.toHaveBeenCalled();
});

it("overlaps source reads with ordered writes for a multi-megabyte legacy transfer", async () => {
  vi.useFakeTimers();
  try {
    const size = 4_872_462;
    const chunkSize = 6 * 1024;
    const count = Math.ceil(size / chunkSize);
    const delay = 20;
    const started = Date.now();
    let reads = 0;
    let writes = 0;
    let maxReads = 0;
    let maxWrites = 0;
    let expectedOffset = 0;
    const pauses = (ms: number) =>
      new Promise((resolve) => setTimeout(resolve, ms));
    vi.mocked(hostControl.exportSessionChunk).mockImplementation(
      async (_host, _session, _transfer, offset) => {
        maxReads = Math.max(maxReads, ++reads);
        await pauses(delay);
        reads -= 1;
        const end = Math.min(offset + chunkSize, size);
        return {
          kind: "migration",
          manifest: { ...manifest, size },
          b64: toBase64Url(new Uint8Array(end - offset)),
          next: end < size ? end : null,
        };
      },
    );
    vi.mocked(hostControl.importSessionChunk).mockImplementation(
      async (_host, chunk) => {
        maxWrites = Math.max(maxWrites, ++writes);
        expect(chunk.offset).toBe(expectedOffset);
        await pauses(delay);
        writes -= 1;
        expectedOffset = Math.min(expectedOffset + chunkSize, size);
        return expectedOffset === size;
      },
    );
    vi.mocked(hostControl.reopenSession).mockResolvedValue({ delivered: true });
    const migration = migrateSession(args);
    await vi.runAllTimersAsync();
    await migration;
    const serialDuration = count * delay * 2;
    expect(Date.now() - started).toBeLessThan(serialDuration * 0.6);
    expect(expectedOffset).toBe(size);
    expect(maxReads).toBe(1);
    expect(maxWrites).toBe(1);
    expect(hostControl.exportSessionChunk).toHaveBeenCalledTimes(count);
    expect(hostControl.importSessionChunk).toHaveBeenCalledTimes(count);
    expect(hostControl.reopenSession).toHaveBeenCalledTimes(1);
  } finally {
    vi.useRealTimers();
  }
});

it.each([false, true])(
  "handles a read-ahead failure (destination write fails: %s)",
  async (writeFails) => {
    vi.useFakeTimers();
    try {
      chunks();
      const readError = new Error("source disconnected");
      const writeError = new Error("destination disconnected");
      vi.mocked(hostControl.exportSessionChunk)
        .mockReset()
        .mockResolvedValueOnce({
          kind: "migration",
          manifest,
          b64: toBase64Url(new Uint8Array([1, 2, 3])),
          next: 3,
        })
        .mockImplementationOnce(
          () =>
            new Promise((_resolve, reject) =>
              setTimeout(() => reject(readError), 5),
            ),
        );
      vi.mocked(hostControl.importSessionChunk)
        .mockReset()
        .mockImplementationOnce(
          () =>
            new Promise((resolve, reject) =>
              setTimeout(
                () => {
                  if (writeFails) reject(writeError);
                  else resolve(false);
                },
                writeFails ? 2 : 10,
              ),
            ),
        );
      const result = migrateSession(args).catch((error) => error);
      await vi.runAllTimersAsync();
      expect(await result).toBe(writeFails ? writeError : readError);
      expect(hostControl.exportSessionChunk).toHaveBeenCalledTimes(2);
      expect(hostControl.importSessionChunk).toHaveBeenCalledTimes(1);
      expect(hostControl.reopenSession).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  },
);
