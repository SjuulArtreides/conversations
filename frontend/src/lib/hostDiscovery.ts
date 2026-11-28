import { loadBrowserKey } from "./hostAuth";
import { hostRtcConfig } from "./hostNetwork";
import { pairedHost, pairHost, type PairedHost } from "./hostPairing";
import { DESCRIPTOR_PREFIX, parseDescriptor } from "./hostStatements";
import { openPrivateChannel, PrivateChannelError } from "./privateChannel";

interface AttachedHost {
  id: string;
  key: string;
}

async function attachedHosts(): Promise<AttachedHost[]> {
  const response = await fetch("/api/host-relay");
  if (!response.ok) return [];
  const body = (await response.json().catch(() => null)) as {
    hosts?: unknown;
  } | null;
  if (!Array.isArray(body?.hosts)) return [];
  return body.hosts.filter(
    (host): host is AttachedHost =>
      typeof host?.id === "string" && typeof host?.key === "string",
  );
}

/**
 * Pair the first machine of this account that approved this browser's key.
 * The server only says which machines the account attached; a machine is
 * paired only after it opens a private channel for this exact key, answering
 * with a statement signed by the key whose hash is its id.
 */
export async function connectApprovedHost(): Promise<PairedHost | null> {
  const key = await loadBrowserKey();
  for (const candidate of await attachedHosts()) {
    if (pairedHost(candidate.id)) continue;
    const descriptor = DESCRIPTOR_PREFIX + candidate.key;
    let host;
    try {
      host = await parseDescriptor(descriptor);
    } catch {
      continue;
    }
    if (host.id !== candidate.id) continue;
    const config = hostRtcConfig(host.id);
    try {
      const channel = await openPrivateChannel({
        host,
        key,
        dlg: null,
        iceServers: config.iceServers,
        relayOnly: config.iceTransportPolicy === "relay",
      });
      channel.close();
    } catch (error) {
      if (error instanceof PrivateChannelError) continue;
      throw error;
    }
    return pairHost(descriptor, "My machine", "browser-key");
  }
  return null;
}
