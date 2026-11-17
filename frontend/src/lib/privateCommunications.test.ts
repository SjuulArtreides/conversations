import { it, expect, vi, beforeEach } from "vitest";
import { readPrivateCommunications } from "./privateCommunications";
const operation = vi.hoisted(() => vi.fn());
vi.mock("./hostControl", () => ({
  hostControl: { privateRead: vi.fn((_host, run) => run({ operation })) },
}));
beforeEach(() => vi.clearAllMocks());
const peer = {
  session_id: "peer",
  host_id: null,
  title: "Peer work",
  agent: "codex",
  last_seen_at: null,
  connected: null,
  state: "unknown",
  reachable: null,
  readable: true,
};
const event = {
  id: "one",
  peer_session_id: "peer",
  host_id: null,
  direction: "sent",
  title: "Review this",
  status: "uncertain",
  created_at: null,
};
it("assembles complete pages, deduplicates replayed events, and preserves uncertain metadata", async () => {
  operation
    .mockResolvedValueOnce({
      body: {
        kind: "communications",
        peers: [peer],
        exchanges: [event],
        next: "received:0",
      },
    })
    .mockResolvedValueOnce({
      body: {
        kind: "communications",
        peers: [peer],
        exchanges: [
          event,
          { ...event, id: "two", direction: "received", status: "revoked" },
        ],
        next: null,
      },
    });
  const result = await readPrivateCommunications("host", "session");
  expect(result.sessions).toHaveLength(1);
  expect(result.sessions[0].href).toBe("/private/peer");
  expect(result.sessions[0].conversation).toMatchObject({
    sent_count: 1,
    received_count: 1,
    last_message_at: null,
    last_direction: null,
    messages: [{ status: "uncertain" }, { status: "revoked" }],
  });
  expect(operation).toHaveBeenNthCalledWith(
    2,
    "communications.list",
    "session",
    { session: "session", after: "received:0", limit: 32 },
  );
});
it("rejects failed later pages without returning a misleading partial summary", async () => {
  operation
    .mockResolvedValueOnce({
      body: {
        kind: "communications",
        peers: [peer],
        exchanges: [event],
        next: "sent:1",
      },
    })
    .mockResolvedValueOnce({
      status: "refused",
      detail: "read grant revoked",
      body: null,
    });
  await expect(readPrivateCommunications("host", "session")).rejects.toThrow(
    "read grant revoked",
  );
});
it("never turns a foreign peer into a local transcript route", async () => {
  operation.mockResolvedValue({
    body: {
      kind: "communications",
      peers: [{ ...peer, host_id: "foreign", readable: false }],
      exchanges: [{ ...event, host_id: "foreign" }],
      next: null,
    },
  });
  expect(
    (await readPrivateCommunications("host", "session")).sessions[0].href,
  ).toBeUndefined();
});
