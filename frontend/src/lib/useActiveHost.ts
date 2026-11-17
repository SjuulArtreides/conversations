import { usePairedHosts } from "@/lib/hostPairing";
import { LOCAL_HOST_ID, useLocalHost } from "@/lib/localHost";

/** The host this browser can currently read, scoped to its active pairing. */
export function useActiveHost(): string | null {
  const local = useLocalHost();
  const { hosts, active } = usePairedHosts();
  if (local.local) return local.authenticated ? LOCAL_HOST_ID : null;
  return hosts.some((host) => host.id === active) ? active : null;
}
