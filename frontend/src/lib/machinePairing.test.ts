import "fake-indexeddb/auto";
import { beforeEach, expect, it, vi } from "vitest";
import { pairMachine } from "./hostControl";
import { pairMyMachines } from "./machinePairing";
import type { PairedHost } from "./hostPairing";
import { makeTestHost } from "./testHost";

vi.mock("./hostControl", () => ({ pairMachine: vi.fn() }));

const keyOf = (descriptor: string) => descriptor.slice("chd1.".length);

async function pinned(label: string): Promise<PairedHost & { key: string }> {
  const host = await makeTestHost();
  return {
    id: host.id,
    key: keyOf(host.descriptor),
    label,
    authMode: "browser-key",
  };
}

function listing(hosts: { id: string; key: string; card?: unknown }[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(JSON.stringify({ hosts }), { status: 200 })),
  );
}

beforeEach(() => vi.mocked(pairMachine).mockReset());

it("pairs every online machine with every other, using this browser's pinned keys", async () => {
  const [mac, laptop, offline] = await Promise.all([
    pinned("Mac"),
    pinned("Laptop"),
    pinned("Offline"),
  ]);
  // The server lists another key for the laptop; the pinned one is used.
  listing([
    { id: mac.id, key: mac.key, card: { p: "mac-p", s: "mac-s" } },
    { id: laptop.id, key: mac.key, card: { p: "lap-p", s: "lap-s" } },
  ]);
  vi.mocked(pairMachine).mockResolvedValue("Laptop");

  const results = await pairMyMachines([mac, laptop, offline]);

  expect(vi.mocked(pairMachine).mock.calls).toEqual([
    [mac.id, { key: laptop.key, card: { p: "lap-p", s: "lap-s" } }],
    [laptop.id, { key: mac.key, card: { p: "mac-p", s: "mac-s" } }],
  ]);
  expect(results).toEqual([
    { host: "Mac", peer: "Laptop" },
    { host: "Laptop", peer: "Mac" },
  ]);
});

it("reports a machine that refuses, and needs two online machines", async () => {
  const [mac, laptop] = await Promise.all([pinned("Mac"), pinned("Laptop")]);
  listing([
    { id: mac.id, key: mac.key, card: { p: "a", s: "b" } },
    { id: laptop.id, key: laptop.key, card: { p: "c", s: "d" } },
  ]);
  vi.mocked(pairMachine)
    .mockRejectedValueOnce(new Error("needs full control"))
    .mockResolvedValueOnce("Mac");
  expect(await pairMyMachines([mac, laptop])).toEqual([
    { host: "Mac", peer: "Laptop", error: "needs full control" },
    { host: "Laptop", peer: "Mac" },
  ]);

  listing([{ id: mac.id, key: mac.key, card: { p: "a", s: "b" } }]);
  await expect(pairMyMachines([mac, laptop])).rejects.toThrow(/at least two/);
});
