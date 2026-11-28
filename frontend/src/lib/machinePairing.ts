import { pairMachine } from "./hostControl";
import { isPinned, usePairedHosts, type PairedHost } from "./hostPairing";

interface Card {
  p: string;
  s: string;
}

export interface MachinePairResult {
  host: string;
  peer: string;
  error?: string;
}

async function onlineCards(): Promise<Map<string, Card>> {
  const response = await fetch("/api/host-relay");
  if (!response.ok) return new Map();
  const body = (await response.json().catch(() => null)) as {
    hosts?: { id?: unknown; card?: { p?: unknown; s?: unknown } }[];
  } | null;
  const cards = new Map<string, Card>();
  for (const host of body?.hosts ?? []) {
    if (
      typeof host.id === "string" &&
      typeof host.card?.p === "string" &&
      typeof host.card?.s === "string"
    )
      cards.set(host.id, { p: host.card.p, s: host.card.s });
  }
  return cards;
}

/**
 * Pair every machine this browser has pinned with every other one that is
 * online, so each has full control of the others. Identity keys come from
 * this browser's own pins; the server only supplies the cards, which each
 * host verifies against the pinned key it is given.
 */
export async function pairMyMachines(
  hosts: PairedHost[],
): Promise<MachinePairResult[]> {
  const cards = await onlineCards();
  const online = hosts.filter(isPinned).filter((h) => cards.has(h.id));
  if (online.length < 2)
    throw new Error(
      "Connect this browser to at least two of your machines, and keep them online.",
    );
  const results: MachinePairResult[] = [];
  for (const host of online) {
    for (const peer of online) {
      if (peer.id === host.id) continue;
      try {
        await pairMachine(host.id, {
          key: peer.key,
          card: cards.get(peer.id)!,
        });
        results.push({ host: host.label, peer: peer.label });
      } catch (error) {
        results.push({
          host: host.label,
          peer: peer.label,
          error: (error as Error).message,
        });
      }
    }
  }
  return results;
}

export function usePinnedHosts(): PairedHost[] {
  return usePairedHosts().hosts.filter(isPinned);
}
