import { it, expect, vi, beforeEach } from "vitest";
import {
  privateNotifications,
  requestPrivateAttention,
  privateNotificationAction,
} from "./privateAttention";
const operation = vi.hoisted(() => vi.fn());
vi.mock("./hostControl", () => ({
  hostControl: { privateRead: vi.fn((_host, run) => run({ operation })) },
}));
beforeEach(() => vi.clearAllMocks());
it("reads all inbox pages and rejects a nonadvancing cursor", async () => {
  operation
    .mockResolvedValueOnce({
      body: {
        kind: "notifications",
        notifications: [{ id: "first" }],
        next: 1,
      },
    })
    .mockResolvedValueOnce({
      body: {
        kind: "notifications",
        notifications: [{ id: "second" }],
        next: null,
      },
    });
  expect(await privateNotifications("host")).toEqual([
    { id: "first" },
    { id: "second" },
  ]);
  expect(operation).toHaveBeenNthCalledWith(2, "notifications.list", "host", {
    after: 1,
    limit: 20,
  });
  operation.mockResolvedValue({
    body: { kind: "notifications", notifications: [], next: 1 },
  });
  await expect(privateNotifications("host")).rejects.toThrow("repeated");
});
it("preserves explicit local-owner identity, idempotency, and host refusal", async () => {
  operation.mockResolvedValue({
    status: "refused",
    detail: "read grant revoked",
    body: null,
  });
  await expect(
    requestPrivateAttention(
      "host",
      "s",
      { kind: "local_owner" },
      "Please review",
      "uuid",
    ),
  ).rejects.toThrow("read grant revoked");
  expect(operation).toHaveBeenCalledWith("attention.request", "s", {
    session: "s",
    recipient: { kind: "local_owner" },
    message: "Please review",
    idempotency_key: "uuid",
  });
  await expect(
    privateNotificationAction("host", "s", "notification", true),
  ).rejects.toThrow("read grant revoked");
  expect(operation).toHaveBeenLastCalledWith("notifications.resolve", "s", {
    session: "s",
    id: "notification",
  });
});
