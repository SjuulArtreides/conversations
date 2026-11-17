import { expect, it, vi, afterEach } from "vitest";
import { readPrivatePrCache } from "./privatePrCache";
import { hostControl } from "./hostControl";
import { toBase64Url } from "./hostAuth";
vi.mock("./hostControl", () => ({ hostControl: { privateRead: vi.fn() } }));
afterEach(() => vi.resetAllMocks());
async function snapshot(body: string) {
  const value = {
    prs: [
      {
        url: "https://github.com/a/b/pull/1",
        owner: "a",
        repo: "b",
        number: 1,
        status: null,
      },
    ],
    comments: [
      {
        owner: "a",
        repo: "b",
        number: 1,
        comment_id: "c",
        kind: "issue",
        author: "dev",
        body,
        html_url: "https://github.com/a/b/pull/1#issuecomment-1",
        created_at: "now",
        ingested_at: "now",
      },
    ],
  };
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  const sha256 = toBase64Url(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
  );
  return { value, bytes, sha256 };
}
function chunk(bytes: Uint8Array, sha256: string, next: number | null) {
  return {
    status: "executed",
    detail: null,
    body: {
      kind: "pr_cache",
      b64: btoa(Array.from(bytes, (b) => String.fromCharCode(b)).join("")),
      sha256,
      next,
    },
  };
}
it("assembles full multi-chunk Unicode comments and reuses the verified snapshot by digest", async () => {
  const data = await snapshot("é😀".repeat(12_000));
  const operation = vi.fn(async (_op, _session, args) => {
    const offset = args.offset as number;
    const end = Math.min(offset + 24_576, data.bytes.length);
    return chunk(
      data.bytes.slice(offset, end),
      data.sha256,
      end === data.bytes.length ? null : end,
    );
  });
  vi.mocked(hostControl.privateRead).mockImplementation(async (_host, read) =>
    read({ operation } as never),
  );
  const value = await readPrivatePrCache("host-large", "s");
  expect(value).toEqual(data.value);
  expect(operation.mock.calls.length).toBeGreaterThan(1);
  const previous = operation.mock.calls.length;
  expect(await readPrivatePrCache("host-large", "s")).toBe(value);
  expect(operation.mock.calls.length).toBe(previous + 1);
});
it("restarts once after a changed snapshot and rejects nonadvancing chunks", async () => {
  const data = await snapshot("complete");
  const operation = vi
    .fn()
    .mockResolvedValueOnce({
      status: "refused",
      detail: "PR cache changed; restart from offset 0",
      body: null,
    })
    .mockResolvedValueOnce(chunk(data.bytes, data.sha256, null));
  vi.mocked(hostControl.privateRead).mockImplementation(async (_host, read) =>
    read({ operation } as never),
  );
  expect(await readPrivatePrCache("host-change", "s")).toEqual(data.value);
  expect(operation.mock.calls.map((call) => call[2].offset)).toEqual([0, 0]);
  operation.mockResolvedValue(chunk(new Uint8Array([1]), data.sha256, 0));
  await expect(readPrivatePrCache("host-bad", "s")).rejects.toThrow(
    "cursor did not advance",
  );
});
it("rejects corrupted cache bytes rather than exposing unverified comments", async () => {
  const data = await snapshot("original");
  const operation = vi
    .fn()
    .mockResolvedValue(
      chunk(
        new TextEncoder().encode('{"prs":[],"comments":[]}'),
        data.sha256,
        null,
      ),
    );
  vi.mocked(hostControl.privateRead).mockImplementation(async (_host, read) =>
    read({ operation } as never),
  );
  await expect(readPrivatePrCache("host-corrupt", "s")).rejects.toThrow(
    "digest did not match",
  );
});
