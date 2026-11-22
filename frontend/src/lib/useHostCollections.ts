import { useQueries } from "@tanstack/react-query";
import { usePairedHosts, isPinned } from "./hostPairing";
import { LOCAL_HOST_ID, useLocalHost } from "./localHost";
import { getDelegation } from "./hostAuth";
import { hostControl, hostOnline } from "./hostControl";
import { listSessions } from "./privateChannel";
import { filterClosing, privateScope } from "./pendingClose";

/** Read every trusted host independently of the new-session destination. */
export function useHostCollections() {
  const local = useLocalHost();
  const paired = usePairedHosts();
  const hosts = local.local
    ? local.authenticated
      ? [
          {
            id: LOCAL_HOST_ID,
            label: "This machine",
            key: "local",
            authMode: "browser-key" as const,
          },
        ]
      : []
    : paired.hosts;
  const access = useQueries({
    queries: hosts.map((host) => ({
      queryKey: ["host-delegation", host.id],
      queryFn: () => getDelegation(host.id),
      enabled:
        !local.local && isPinned(host) && host.authMode !== "browser-key",
      retry: false,
    })),
  });
  const online = useQueries({
    queries: hosts.map((host) => ({
      queryKey: ["host-status", host.id],
      queryFn: () => hostOnline(host.id),
      enabled:
        !local.local && isPinned(host) && host.authMode !== "browser-key",
      retry: false,
      refetchInterval: 5000,
    })),
  });
  const allowed = hosts.map(
    (host, i) =>
      local.local ||
      (isPinned(host) && (host.authMode === "browser-key" || !!access[i].data)),
  );
  const queries = useQueries({
    queries: hosts.flatMap((host, i) =>
      [false, true].map((running) => ({
        queryKey: [
          "private",
          host.id,
          running ? "active-sessions" : "sessions",
        ],
        queryFn: async () => {
          const sessions = await hostControl.privateRead(
            host.id,
            (channel) => listSessions(channel, { runningOnly: running }),
            { allowUnlock: false },
          );
          return running
            ? filterClosing(privateScope(host.id), sessions)
            : sessions;
        },
        enabled: allowed[i],
        retry: false,
        staleTime: running ? 0 : 300_000,
        refetchInterval: running ? 5000 : (false as const),
      })),
    ),
  });
  return hosts.map((host, i) => {
    const archive = queries[i * 2],
      active = queries[i * 2 + 1];
    const live = new Map(
      (allowed[i] ? (active.data ?? []) : []).map((session) => [
        session.id,
        session,
      ]),
    );
    const data = (allowed[i] ? (archive.data ?? []) : [])
      .map((session) => {
        const running = live.get(session.id);
        live.delete(session.id);
        return (
          running ??
          (active.data && session.running
            ? { ...session, running: false }
            : session)
        );
      })
      .concat([...live.values()]);
    return {
      host,
      needsRepair: !local.local && !isPinned(host),
      locked:
        !local.local &&
        isPinned(host) &&
        host.authMode !== "browser-key" &&
        !access[i].isLoading &&
        !access[i].data,
      access: access[i],
      online: online[i],
      data: data.map((session) => ({
        ...session,
        hostId: host.id,
        hostLabel: host.label,
      })),
      isLoading:
        allowed[i] && !data.length && (archive.isLoading || active.isLoading),
      isError: allowed[i] && !data.length && archive.isError && active.isError,
      error: active.error ?? archive.error,
      refetch: () => Promise.all([archive.refetch(), active.refetch()]),
    };
  });
}
