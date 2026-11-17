import { hostControl } from "./hostControl";
import { HOST_SCOPE } from "./hostAuth";
export type AttentionSubject =
  { kind: "credential"; kid: string } | { kind: "local_owner" };
export interface AttentionIdentity {
  subject: AttentionSubject;
  label: string;
}
export interface HostNotification {
  id: string;
  session_id: string;
  session_title: string | null;
  kind: "attention";
  sender: AttentionIdentity;
  recipient: AttentionSubject;
  message: string;
  created_at: string;
  read_at: string | null;
  resolved_at: string | null;
}
export async function privateAttentionRecipients(
  hostId: string,
  session: string,
) {
  const result = await hostControl.privateRead(hostId, (channel) =>
    channel.operation("attention.recipients", session, { session }),
  );
  if (result.body?.kind !== "attention_recipients")
    throw new Error(result.detail ?? "The host could not list recipients.");
  return result.body.recipients;
}
export async function requestPrivateAttention(
  hostId: string,
  session: string,
  recipient: AttentionSubject,
  message: string,
  idempotency_key: string,
) {
  const result = await hostControl.privateRead(
    hostId,
    (channel) =>
      channel.operation("attention.request", session, {
        session,
        recipient,
        message,
        idempotency_key,
      }),
    { allowUnlock: true },
  );
  if (result.status !== "executed" && result.status !== "replayed")
    throw new Error(result.detail ?? "The host could not request attention.");
}
export async function privateNotifications(
  hostId: string,
): Promise<HostNotification[]> {
  return hostControl.privateRead(
    hostId,
    async (channel) => {
      const notifications: HostNotification[] = [];
      const seen = new Set<number>();
      let after: number | null = null;
      do {
        const result = await channel.operation(
          "notifications.list",
          HOST_SCOPE,
          {
            after,
            limit: 20,
          },
        );
        if (result.body?.kind !== "notifications")
          throw new Error(result.detail ?? "The host inbox is unavailable.");
        notifications.push(...result.body.notifications);
        after = result.body.next;
        if (after !== null) {
          if (seen.has(after))
            throw new Error("The host repeated an inbox page.");
          seen.add(after);
        }
      } while (after !== null);
      return notifications;
    },
    { allowUnlock: false },
  );
}
export async function privateNotificationAction(
  hostId: string,
  session: string,
  id: string,
  resolve: boolean,
) {
  const result = await hostControl.privateRead(
    hostId,
    (channel) =>
      channel.operation(
        resolve ? "notifications.resolve" : "notifications.read",
        session,
        { session, id },
      ),
    { allowUnlock: true },
  );
  if (result.status !== "executed" && result.status !== "replayed")
    throw new Error(
      result.detail ?? "The host could not update this notification.",
    );
}
