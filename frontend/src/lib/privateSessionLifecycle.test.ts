import { afterEach, expect, it, vi } from "vitest";
import { restartPrivateSession } from "./privateSessionLifecycle";
import { hostControl } from "./hostControl";
vi.mock("./hostControl", () => ({
  hostControl: {
    endSession: vi.fn(),
    reopenSession: vi.fn(),
    privateRead: vi.fn(),
  },
}));
afterEach(() => {
  vi.useRealTimers();
  vi.resetAllMocks();
});
it("waits for confirmed stop before reopen and confirmed running before completion", async () => {
  vi.useFakeTimers();
  const active = [{ id: "s", running: true }];
  vi.mocked(hostControl.privateRead)
    .mockResolvedValueOnce(active)
    .mockResolvedValueOnce([])
    .mockResolvedValueOnce([])
    .mockResolvedValueOnce(active);
  let complete = false;
  const job = restartPrivateSession("host-a", "s").then(() => {
    complete = true;
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(hostControl.endSession).toHaveBeenCalledWith("s", "host-a");
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(250);
  expect(hostControl.reopenSession).toHaveBeenCalledWith("s", "host-a");
  expect(complete).toBe(false);
  await vi.advanceTimersByTimeAsync(250);
  await job;
  expect(complete).toBe(true);
});
it("never reopens after the host fails to confirm a stop", async () => {
  vi.useFakeTimers();
  vi.mocked(hostControl.privateRead).mockResolvedValue([
    { id: "s", running: true },
  ]);
  const result = expect(restartPrivateSession("host-a", "s")).rejects.toThrow(
    "not confirmed the session stopped",
  );
  await vi.advanceTimersByTimeAsync(15_000);
  await result;
  expect(hostControl.reopenSession).not.toHaveBeenCalled();
});
