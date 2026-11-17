import { LOCAL_HOST_ID, useLocalHost } from "./localHost";
import { useQuery } from "@tanstack/react-query";
import { hostOnline } from "./hostControl";
import { usePairedHosts } from "./hostPairing";

/** Query key for the paired-host presence poll. */
export const CONTROL_AVAILABLE_QUERY_KEY = ["control-available"] as const;

/** How often to re-poll the relay for the paired host. */
const REFETCH_INTERVAL_MS = 5000;

/**
 * Gates the "Re-open as live" / "Start new session" buttons.
 *
 * `available`: the buttons may be pressed (a host is pinned in `hostPairing`). With no paired host they stay
 *   pressable so the click can explain how to pair (never a silent dead
 *   button); with a paired host they need that host's relay stream to be up.
 *
 * Presence is the relay's view of the host by id, not "some host for this
 * account". Any error counts as offline.
 */
export function useControlAvailable() {
  const { hosts, active: pairedActive } = usePairedHosts();
  const local = useLocalHost();
  const active = local.local
    ? local.authenticated
      ? LOCAL_HOST_ID
      : null
    : pairedActive;
  const query = useQuery<boolean>({
    queryKey: [...CONTROL_AVAILABLE_QUERY_KEY, active],
    queryFn: () => hostOnline(active as string),
    enabled: active !== null,
    refetchInterval: REFETCH_INTERVAL_MS,
    staleTime: REFETCH_INTERVAL_MS,
    retry: false,
  });
  const paired = local.local || hosts.length > 0;
  return {
    available: !paired || query.data === true,
    isLoading: paired && query.isLoading,
  };
}
