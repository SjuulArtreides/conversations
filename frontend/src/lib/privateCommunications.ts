import { hostControl } from "./hostControl";
import type { AgentKind, CommunicationConversation } from "./types";
export interface CommunicationPeerMetadata {
  session_id: string;
  host_id: string | null;
  title: string;
  agent: AgentKind | null;
  last_seen_at: string | null;
  connected: boolean | null;
  state: "live" | "ended" | "unknown";
  reachable: boolean | null;
  readable: boolean;
}
export interface CommunicationExchange {
  id: string;
  peer_session_id: string;
  host_id: string | null;
  direction: "sent" | "received";
  title: string;
  status: string;
  created_at: string | null;
}
export interface CommunicationPage {
  kind: "communications";
  peers: CommunicationPeerMetadata[];
  exchanges: CommunicationExchange[];
  next: string | null;
}
export interface ConversationView extends Omit<
  CommunicationConversation,
  "last_message_at" | "last_direction" | "last_sent_status" | "messages"
> {
  last_message_at: string | null;
  last_direction: "sent" | "received" | null;
  last_sent_status: string | null;
  messages: {
    id: string;
    title: string;
    direction: "sent" | "received";
    created_at: string | null;
    status?: string;
  }[];
}
export interface CommunicationPeerView extends Omit<
  CommunicationPeerMetadata,
  "state"
> {
  state: "live" | "idle" | "ended" | "unknown";
  owner_email?: string | null;
  conversation: ConversationView | null;
  href?: string;
}
export interface CommunicationsView {
  owner_email?: string | null;
  sessions: CommunicationPeerView[];
}
const peerKey = (host: string | null, id: string) => JSON.stringify([host, id]);
export async function readPrivateCommunications(
  hostId: string,
  session: string,
): Promise<CommunicationsView> {
  return hostControl.privateRead(hostId, async (channel) => {
    const peers = new Map<string, CommunicationPeerMetadata>();
    const exchanges = new Map<string, CommunicationExchange>();
    const cursors = new Set<string>();
    let after: string | null = null;
    do {
      const result = await channel.operation("communications.list", session, {
        session,
        after,
        limit: 32,
      });
      if (result.body?.kind !== "communications")
        throw new Error(
          result.detail ?? "The host's communication metadata is unavailable.",
        );
      for (const peer of result.body.peers)
        peers.set(peerKey(peer.host_id, peer.session_id), peer);
      for (const exchange of result.body.exchanges)
        exchanges.set(
          JSON.stringify([exchange.direction, exchange.id]),
          exchange,
        );
      after = result.body.next;
      if (after !== null) {
        if (cursors.has(after))
          throw new Error("The host repeated a communication page.");
        cursors.add(after);
      }
    } while (after !== null);
    return {
      sessions: [...peers.values()].map((peer) => {
        const messages = [...exchanges.values()].filter(
          (e) =>
            peerKey(e.host_id, e.peer_session_id) ===
            peerKey(peer.host_id, peer.session_id),
        );
        const dated = messages
          .filter((e) => e.created_at !== null)
          .sort((a, b) => a.created_at!.localeCompare(b.created_at!));
        const last = dated.at(-1);
        const sent = messages.filter((e) => e.direction === "sent");
        const received = messages.filter((e) => e.direction === "received");
        const lastSent = [...sent]
          .filter((e) => e.created_at !== null)
          .sort((a, b) => a.created_at!.localeCompare(b.created_at!))
          .at(-1);
        const conversation: ConversationView | null = messages.length
          ? {
              sent_count: sent.length,
              received_count: received.length,
              last_message_at: last?.created_at ?? null,
              last_direction: last?.direction ?? null,
              last_sent_status: lastSent?.status ?? null,
              unacknowledged_incoming: received.filter(
                (e) => e.status === "received",
              ).length,
              messages: messages.map((e) => ({
                id: `${e.direction}:${e.id}`,
                title: e.title,
                direction: e.direction,
                created_at: e.created_at,
                status: e.status,
              })),
            }
          : null;
        return {
          ...peer,
          conversation,
          href:
            peer.readable && peer.host_id === null
              ? `/private/${encodeURIComponent(peer.session_id)}`
              : undefined,
        };
      }),
    };
  });
}
