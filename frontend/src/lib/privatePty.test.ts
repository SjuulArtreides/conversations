import { beforeEach, describe, expect, it, vi } from "vitest";
import type { TerminalHandlers } from "./hostControl";

const { mockSubscribe, mockClose } = vi.hoisted(() => ({
  mockSubscribe: vi.fn(),
  mockClose: vi.fn(),
}));
vi.mock("./hostControl", async (actual) => ({
  ...(await actual<typeof import("./hostControl")>()),
  hostControl: { subscribeTerminal: mockSubscribe },
}));

import { HostControlError } from "./hostControl";
import { openPrivatePty } from "./privatePty";

const CLEAR = [
  0x1b, 0x5b, 0x48, 0x1b, 0x5b, 0x32, 0x4a, 0x1b, 0x5b, 0x33, 0x4a,
];

let host: TerminalHandlers;
const handlers = () => ({
  onFrame: vi.fn(),
  onOpen: vi.fn(),
  onReplayComplete: vi.fn(),
});

beforeEach(() => {
  vi.clearAllMocks();
  mockSubscribe.mockImplementation(
    async (_host: string, _session: string, h: TerminalHandlers) => {
      host = h;
      return { sub: 1, close: mockClose };
    },
  );
});

describe("openPrivatePty", () => {
  it("signals the viewer the terminal is live as soon as subscribe resolves, even without a snapshot", async () => {
    // A private session the host admits to its list but whose first screen
    // frame has not yet reached this peer must not leave the viewer stuck on
    // "Loading terminal…" forever. Firing onOpen + onReplayComplete on
    // subscribe-success lets the LiveTerminal clear its spinner and render an
    // empty grid; a later snapshot still repaints via CLEAR + bytes.
    const h = handlers();
    openPrivatePty("host-a", "s1", h);
    await vi.waitFor(() => expect(h.onOpen).toHaveBeenCalledTimes(1));
    expect(h.onReplayComplete).toHaveBeenCalledTimes(1);
    expect(h.onFrame).not.toHaveBeenCalled();
    // A snapshot still paints the current screen on top of the empty grid.
    host.onFrame({
      kind: "snapshot",
      cols: 80,
      rows: 24,
      data: new Uint8Array([65]),
    });
    expect(h.onFrame).toHaveBeenCalledWith(
      80,
      24,
      new Uint8Array([...CLEAR, 65]),
    );
    expect(h.onOpen).toHaveBeenCalledTimes(1);
  });

  it("subscribes on the named host and treats the first snapshot as the replay boundary", async () => {
    const h = handlers();
    openPrivatePty("host-a", "s1", h);
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalled());
    expect(mockSubscribe.mock.calls[0].slice(0, 2)).toEqual(["host-a", "s1"]);
    // Nothing before the snapshot: an update alone has nothing to apply to.
    host.onFrame({
      kind: "update",
      cols: 80,
      rows: 24,
      data: new Uint8Array([9]),
    });
    expect(h.onFrame).not.toHaveBeenCalled();
    host.onFrame({
      kind: "snapshot",
      cols: 100,
      rows: 30,
      data: new Uint8Array([65, 66]),
    });
    expect(h.onOpen).toHaveBeenCalledTimes(1);
    expect(h.onFrame).toHaveBeenCalledWith(
      100,
      30,
      new Uint8Array([...CLEAR, 65, 66]),
    );
    expect(h.onReplayComplete).toHaveBeenCalledTimes(1);
    host.onFrame({
      kind: "update",
      cols: 100,
      rows: 30,
      data: new Uint8Array([67]),
    });
    expect(h.onFrame).toHaveBeenLastCalledWith(100, 30, new Uint8Array([67]));
    // A later snapshot (after host-side lag) clears and repaints again.
    host.onFrame({
      kind: "snapshot",
      cols: 100,
      rows: 30,
      data: new Uint8Array([68]),
    });
    expect(h.onFrame).toHaveBeenLastCalledWith(
      100,
      30,
      new Uint8Array([...CLEAR, 68]),
    );
    expect(h.onOpen).toHaveBeenCalledTimes(1);
  });

  it("ends cleanly when the terminal ends on the host, and ignores later frames", async () => {
    const h = handlers();
    const pty = openPrivatePty("host-a", "s1", h);
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalled());
    host.onEnd({ reason: "gone", detail: "The terminal ended on your host." });
    await expect(pty.closed).resolves.toEqual({ reason: "eof" });
    host.onFrame({
      kind: "snapshot",
      cols: 1,
      rows: 1,
      data: new Uint8Array([1]),
    });
    expect(h.onFrame).not.toHaveBeenCalled();
  });

  const snapshot = (byte: number) => ({
    kind: "snapshot" as const,
    cols: 80,
    rows: 24,
    data: new Uint8Array([byte]),
  });

  it("follows a lapsed lease with one fresh subscribe and its snapshot, and stops at the next lapse", async () => {
    const h = handlers();
    const pty = openPrivatePty("host-a", "s1", h);
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalledTimes(1));
    const first = host;
    first.onFrame(snapshot(1));
    first.onEnd({ reason: "expired", detail: "The terminal lease lapsed." });
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalledTimes(2));
    expect(mockSubscribe.mock.calls[1].slice(0, 2)).toEqual(["host-a", "s1"]);
    const second = host;
    // The old subscription's late frames and end are not the fresh one's.
    first.onFrame(snapshot(9));
    first.onEnd({ reason: "gone", detail: "late" });
    second.onFrame(snapshot(2));
    expect(h.onFrame).toHaveBeenLastCalledWith(
      80,
      24,
      new Uint8Array([...CLEAR, 2]),
    );
    expect(h.onFrame).toHaveBeenCalledTimes(2);
    expect(h.onOpen).toHaveBeenCalledTimes(1);
    second.onEnd({ reason: "expired", detail: "The terminal lease lapsed." });
    await expect(pty.closed).rejects.toMatchObject({
      reason: "expired",
      code: "refused",
      message: "The terminal lease lapsed.",
    });
    await new Promise((r) => setTimeout(r, 20));
    expect(mockSubscribe).toHaveBeenCalledTimes(2);
  });

  it.each([
    ["denied", "refused"],
    ["protocol", "untrusted"],
    ["connection", "transport"],
  ] as const)(
    "stops on a %s end with code %s and subscribes nothing more by itself",
    async (reason, code) => {
      const pty = openPrivatePty("host-a", "s1", handlers());
      await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalledTimes(1));
      host.onEnd({ reason, detail: `why: ${reason}` });
      await expect(pty.closed).rejects.toMatchObject({
        reason,
        code,
        message: `why: ${reason}`,
      });
      await new Promise((r) => setTimeout(r, 20));
      expect(mockSubscribe).toHaveBeenCalledTimes(1);
    },
  );

  it("does not resubscribe after a lapse once aborted", async () => {
    const controller = new AbortController();
    const pty = openPrivatePty("host-a", "s1", handlers(), controller.signal);
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalledTimes(1));
    controller.abort();
    host.onEnd({ reason: "expired", detail: "lapsed" });
    await expect(pty.closed).resolves.toEqual({ reason: "aborted" });
    expect(mockSubscribe).toHaveBeenCalledTimes(1);
  });

  it("rejects when the subscription cannot be opened, keeping the error's meaning", async () => {
    mockSubscribe.mockRejectedValueOnce(
      new HostControlError("no route", "transport"),
    );
    const pty = openPrivatePty("host-a", "s1", handlers());
    await expect(pty.closed).rejects.toMatchObject({ code: "transport" });
  });

  it("closes the subscription on abort, even one that arrives late", async () => {
    let resolve!: (sub: { sub: number; close(): void }) => void;
    mockSubscribe.mockReturnValueOnce(new Promise((r) => (resolve = r)));
    const controller = new AbortController();
    const pty = openPrivatePty("host-a", "s1", handlers(), controller.signal);
    controller.abort();
    await expect(pty.closed).resolves.toEqual({ reason: "aborted" });
    resolve({ sub: 2, close: mockClose });
    await vi.waitFor(() => expect(mockClose).toHaveBeenCalledTimes(1));
  });

  it("never carries keystrokes", async () => {
    const pty = openPrivatePty("host-a", "s1", handlers());
    pty.sendInput("bHM=");
    await vi.waitFor(() => expect(mockSubscribe).toHaveBeenCalledTimes(1));
    expect(mockSubscribe).toHaveBeenCalledTimes(1);
  });
});
