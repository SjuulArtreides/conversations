import * as React from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useToast } from "@/components/ui/toast";
import {
  clearHostNetwork,
  HostNetworkError,
  MAX_ICE_SERVERS,
  saveHostNetwork,
  useHostNetwork,
  type HostNetwork,
} from "@/lib/hostNetwork";
import { usePairedHosts } from "@/lib/hostPairing";

interface ServerRow {
  urls: string;
  username: string;
  credential: string;
}

const toRows = (network: HostNetwork): ServerRow[] =>
  network.iceServers.map((server) => ({
    urls: (Array.isArray(server.urls) ? server.urls : [server.urls]).join("\n"),
    username: server.username ?? "",
    credential: typeof server.credential === "string" ? server.credential : "",
  }));

/** Form rows back to `RTCIceServer`s; validation is `saveHostNetwork`'s. */
const toServers = (rows: ServerRow[]): RTCIceServer[] =>
  rows.map((row) => {
    const urls = row.urls
      .split("\n")
      .map((url) => url.trim())
      .filter((url) => url !== "");
    const server: RTCIceServer = { urls: urls.length === 1 ? urls[0] : urls };
    if (row.username !== "") server.username = row.username;
    if (row.credential !== "") server.credential = row.credential;
    return server;
  });

function describe(network: HostNetwork): string {
  if (network.iceServers.length === 0) return "Direct connections only.";
  const servers = `${network.iceServers.length} ICE server${network.iceServers.length === 1 ? "" : "s"}`;
  return network.iceTransportPolicy === "relay"
    ? `Relay only, through ${servers}.`
    : `Direct, or through ${servers}.`;
}

// A fresh form for every saved value; the hook keeps a value's identity until
// the stored settings change.
const formKeys = new WeakMap<object, number>();
let nextFormKey = 0;
const formKey = (value: object) => {
  if (!formKeys.has(value)) formKeys.set(value, nextFormKey++);
  return formKeys.get(value)!;
};

/** How this browser reaches one paired host: STUN/TURN servers and whether to
 * relay only. Kept in this browser; renders nothing for an unpaired host. */
export function HostNetworkSettings({ hostId }: { hostId: string }) {
  const { hosts } = usePairedHosts();
  const network = useHostNetwork(hostId);
  if (!hosts.some((host) => host.id === hostId)) return null;

  return (
    <section aria-labelledby="host-network-heading" className="space-y-3">
      <h3 id="host-network-heading" className="text-sm font-semibold">
        Network
      </h3>
      <p className="text-xs text-muted-foreground">
        STUN and TURN servers for reaching this host. They are stored only in
        this browser for this host and never sent to the Conversations server.
        Empty means direct connections only.
      </p>
      {network instanceof HostNetworkError ? (
        <div className="flex items-center gap-2">
          <p role="alert" className="text-sm text-destructive">
            {network.message}
          </p>
          <Button size="sm" variant="outline" onClick={() => clearHostNetwork(hostId)}>
            Clear
          </Button>
        </div>
      ) : (
        <NetworkForm key={formKey(network)} hostId={hostId} network={network} />
      )}
    </section>
  );
}

function NetworkForm({ hostId, network }: { hostId: string; network: HostNetwork }) {
  const { toast } = useToast();
  const [rows, setRows] = React.useState(() => toRows(network));
  const [relay, setRelay] = React.useState(network.iceTransportPolicy === "relay");
  const [error, setError] = React.useState<string | null>(null);

  const update = (index: number, field: keyof ServerRow, value: string) =>
    setRows((current) =>
      current.map((row, i) => (i === index ? { ...row, [field]: value } : row)),
    );

  const save = (event: React.FormEvent) => {
    event.preventDefault();
    try {
      saveHostNetwork(hostId, {
        iceServers: toServers(rows),
        iceTransportPolicy: relay ? "relay" : "all",
      });
      setError(null);
      toast("Network settings saved.", "default");
    } catch (caught) {
      setError(
        caught instanceof HostNetworkError
          ? caught.message
          : "Could not save the network settings.",
      );
    }
  };

  return (
    <form onSubmit={save} className="space-y-3">
      <p data-testid="network-mode" className="text-sm">
        {describe(network)}
      </p>
      {rows.map((row, index) => (
        <fieldset
          key={index}
          data-testid={`ice-server-${index}`}
          className="space-y-2 rounded-md border border-border p-3"
        >
          <legend className="px-1 text-xs text-muted-foreground">
            Server {index + 1}
          </legend>
          <label className="block space-y-1 text-xs">
            <span>URLs (one per line)</span>
            <textarea
              rows={2}
              value={row.urls}
              placeholder="turn:turn.example.com:3478?transport=udp"
              onChange={(event) => update(index, "urls", event.target.value)}
              className="flex w-full rounded-md border border-input bg-card/60 px-3 py-1 font-mono text-sm"
            />
          </label>
          <label className="block space-y-1 text-xs">
            <span>Username</span>
            <Input
              value={row.username}
              autoComplete="off"
              onChange={(event) => update(index, "username", event.target.value)}
            />
          </label>
          <label className="block space-y-1 text-xs">
            <span>Credential</span>
            <Input
              type="password"
              value={row.credential}
              autoComplete="new-password"
              onChange={(event) => update(index, "credential", event.target.value)}
            />
          </label>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            onClick={() => setRows((current) => current.filter((_, i) => i !== index))}
          >
            Remove
          </Button>
        </fieldset>
      ))}
      <label className="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={relay}
          onChange={(event) => setRelay(event.target.checked)}
        />
        Relay only (always connect through TURN)
      </label>
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      <div className="flex gap-2">
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={rows.length >= MAX_ICE_SERVERS}
          onClick={() =>
            setRows((current) => [...current, { urls: "", username: "", credential: "" }])
          }
        >
          Add server
        </Button>
        <Button type="submit" size="sm">
          Save network settings
        </Button>
      </div>
    </form>
  );
}
