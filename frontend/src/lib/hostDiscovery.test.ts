import "fake-indexeddb/auto";
import { beforeEach, expect, it, vi } from "vitest";
import { connectApprovedHost } from "./hostDiscovery";
import { activeHost, pairedHost } from "./hostPairing";
import { makeTestHost } from "./testHost";
import { openPrivateChannel, PrivateChannelError } from "./privateChannel";

vi.mock("./privateChannel", async (original) => {
  const actual = await original<typeof import("./privateChannel")>();
  return { ...actual, openPrivateChannel: vi.fn() };
});

function listing(hosts: { id: string; key: string }[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      expect(url).toBe("/api/host-relay");
      return new Response(JSON.stringify({ hosts }), { status: 200 });
    }),
  );
}

const keyOf = (descriptor: string) => descriptor.slice("chd1.".length);

beforeEach(() => {
  localStorage.clear();
  vi.mocked(openPrivateChannel).mockReset();
});

it("pairs the account's machine that opens a channel for this browser key", async () => {
  const refusing = await makeTestHost();
  const approving = await makeTestHost();
  listing([
    { id: refusing.id, key: keyOf(refusing.descriptor) },
    { id: approving.id, key: keyOf(approving.descriptor) },
  ]);
  const close = vi.fn();
  vi.mocked(openPrivateChannel).mockImplementation(async ({ host, dlg }) => {
    expect(dlg).toBeNull();
    if (host.id === refusing.id)
      throw new PrivateChannelError(
        "unknown, revoked or expired credential",
        "refused",
      );
    return { close } as unknown as Awaited<
      ReturnType<typeof openPrivateChannel>
    >;
  });

  const paired = await connectApprovedHost();

  expect(paired?.id).toBe(approving.id);
  expect(activeHost()).toMatchObject({
    id: approving.id,
    key: keyOf(approving.descriptor),
    authMode: "browser-key",
  });
  expect(pairedHost(refusing.id)).toBeNull();
  expect(close).toHaveBeenCalled();
});

it("pairs nothing while no machine has approved the key", async () => {
  const host = await makeTestHost();
  listing([{ id: host.id, key: keyOf(host.descriptor) }]);
  vi.mocked(openPrivateChannel).mockRejectedValue(
    new PrivateChannelError("unknown credential", "refused"),
  );

  expect(await connectApprovedHost()).toBeNull();
  expect(activeHost()).toBeNull();
});

it("never asks a listed id that is not the hash of its key", async () => {
  const real = await makeTestHost();
  const other = await makeTestHost();
  listing([{ id: real.id, key: keyOf(other.descriptor) }]);

  expect(await connectApprovedHost()).toBeNull();
  expect(openPrivateChannel).not.toHaveBeenCalled();
  expect(activeHost()).toBeNull();
});
