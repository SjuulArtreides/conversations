import { describe, it, expect, vi, beforeEach } from "vitest";
import { privateSessionLinearSource } from "./sessionLinearSource";
import { hostControl } from "./hostControl";
const operation = vi.fn();
vi.mock("./hostControl", () => ({
  hostControl: { privateRead: vi.fn((_host, run) => run({ operation })) },
}));
const doc = (id: string) => ({
  url: `https://linear.app/team/issue/${id}`,
  workspace: "team",
  kind: "issue",
  identifier: id,
});
beforeEach(() => vi.clearAllMocks());
describe("private Linear references", () => {
  it("keeps complete URL-cursor pages on the selected host", async () => {
    operation
      .mockResolvedValueOnce({
        body: { kind: "linear", docs: [doc("T-1")], next: doc("T-1").url },
      })
      .mockResolvedValueOnce({
        body: { kind: "linear", docs: [doc("T-2")], next: null },
      });
    expect(await privateSessionLinearSource("laptop").list("session")).toEqual([
      doc("T-1"),
      doc("T-2"),
    ]);
    expect(hostControl.privateRead).toHaveBeenCalledWith(
      "laptop",
      expect.any(Function),
    );
    expect(operation).toHaveBeenNthCalledWith(2, "linear.list", "session", {
      session: "session",
      after: doc("T-1").url,
      limit: 32,
    });
  });
  it("rejects repeating cursors rather than looping or silently truncating", async () => {
    operation.mockResolvedValue({
      body: { kind: "linear", docs: [doc("T-1")], next: "repeated" },
    });
    await expect(
      privateSessionLinearSource("host").list("session"),
    ).rejects.toThrow("repeated");
    expect(operation).toHaveBeenCalledTimes(2);
  });
  it("reports host refusal and never falls back to central unlink", async () => {
    operation.mockResolvedValue({
      status: "refused",
      detail: "grant revoked",
      body: null,
    });
    await expect(
      privateSessionLinearSource("host").unlink("session", doc("T-1").url),
    ).rejects.toThrow("grant revoked");
    expect(operation).toHaveBeenCalledWith("linear.unlink", "session", {
      session: "session",
      url: doc("T-1").url,
    });
  });
});
