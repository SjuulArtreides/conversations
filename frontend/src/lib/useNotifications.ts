import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./api";
import { useAuth } from "./useAuth";
import { usePairedHosts } from "./hostPairing";
import { LOCAL_HOST_ID, useLocalHost } from "./localHost";
import {
  privateNotifications,
  privateNotificationAction,
} from "./privateAttention";
import type { SessionNotification } from "./types";

export type NotificationItem = Omit<
  SessionNotification,
  "sender_email" | "recipient_email"
> & {
  sender_email?: string;
  recipient_email?: string;
  sender_label?: string;
  hostId?: string;
};
export const notificationSender = (item: NotificationItem) =>
  item.sender_label ?? item.sender_email ?? "Unknown sender";
export function useNotifications() {
  const { user } = useAuth();
  const local = useLocalHost();
  const paired = usePairedHosts();
  const hostId = local.local
    ? local.authenticated
      ? LOCAL_HOST_ID
      : null
    : paired.active;
  const central = useQuery({
    queryKey: ["notifications", user?.email],
    queryFn: api.notifications,
    enabled: Boolean(user) && !local.local,
    refetchInterval: 5000,
  });
  const host = useQuery({
    queryKey: ["notifications", "host", hostId],
    queryFn: () => privateNotifications(hostId!),
    enabled: !!hostId,
    refetchInterval: 5000,
    retry: false,
  });
  const notifications: NotificationItem[] = [
    ...(!local.local && user ? (central.data ?? []) : []),
    ...(hostId && !host.error
      ? (host.data ?? []).map(({ sender, recipient: _recipient, ...item }) => ({
          ...item,
          hostId,
          sender_label: sender.label,
        }))
      : []),
  ].sort((a, b) => b.created_at.localeCompare(a.created_at));
  const errors = [
    ...(!local.local && user && central.error
      ? [`Shared inbox: ${central.error.message}`]
      : []),
    ...(hostId && host.error
      ? [`Host inbox unavailable: ${host.error.message}`]
      : []),
  ];
  return {
    notifications,
    canRequest: local.local ? local.authenticated : Boolean(user) || !!hostId,
    isPending:
      (!local.local && !!user && central.isPending) ||
      (!!hostId && host.isPending),
    isError: errors.length > 0,
    errors,
    refetch: () =>
      Promise.all([
        !local.local && user ? central.refetch() : undefined,
        hostId ? host.refetch() : undefined,
      ]),
  };
}
export function useNotificationAction() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      resolve,
      hostId,
      session_id,
    }: {
      id: string;
      resolve: boolean;
      hostId?: string;
      session_id?: string;
    }) => {
      if (hostId) {
        if (!session_id) throw new Error("Missing host session.");
        return privateNotificationAction(hostId, session_id, id, resolve);
      }
      return resolve ? api.resolveNotification(id) : api.readNotification(id);
    },
    onSuccess: () => client.invalidateQueries({ queryKey: ["notifications"] }),
  });
}
