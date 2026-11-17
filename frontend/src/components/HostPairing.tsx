import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { HostSharing } from "@/components/HostSharing";
import { useToast } from "@/components/ui/toast";
import { forgetDelegation, getDelegation } from "@/lib/hostAuth";
import {
  closePrivateChannels,
  hostOnline,
  unlockAgain,
} from "@/lib/hostControl";
import {
  isPinned,
  setHostAuthMode,
  pairHost,
  unpairHost,
  usePairedHosts,
} from "@/lib/hostPairing";

const DELEGATION_KEY = ["host-delegation"] as const;
const HOST_STATUS_KEY = ["host-status"] as const;

const formatTime = (seconds: number) =>
  new Date(seconds * 1000).toLocaleString();

function HostRow({
  id,
  label,
  pinned,
  authMode,
}: {
  id: string;
  label: string;
  pinned: boolean;
  authMode: "passkey" | "browser-key";
}) {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const [busy, setBusy] = React.useState(false);
  const online = useQuery({
    queryKey: [...HOST_STATUS_KEY, id, authMode],
    queryFn: () => hostOnline(id),
    refetchInterval: 5000,
    retry: false,
  });
  const delegation = useQuery({
    queryKey: [...DELEGATION_KEY, id, authMode],
    queryFn: () => getDelegation(id),
  });

  const unlock = async () => {
    setBusy(true);
    try {
      await unlockAgain(id);
      await queryClient.invalidateQueries({
        queryKey: [...DELEGATION_KEY, id, authMode],
      });
      toast("Unlocked for 24 hours.", "default");
    } catch (error) {
      toast((error as Error).message, "error");
    } finally {
      setBusy(false);
    }
  };

  const lock = async () => {
    await forgetDelegation(id);
    await queryClient.invalidateQueries({ queryKey: [...DELEGATION_KEY, id] });
  };

  return (
    <li className="rounded-md border border-border p-3 text-sm">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <div className="font-medium">{label}</div>
          <code
            className="block max-w-full break-all text-xs text-muted-foreground"
            data-testid="host-id"
          >
            {id}
          </code>
        </div>
        <span
          className={
            online.data === true
              ? "text-xs text-green-600"
              : "text-xs text-muted-foreground"
          }
          data-testid="host-online"
        >
          {online.isPending
            ? "Checking host…"
            : online.isError
              ? "Connection check failed"
              : online.data === true
                ? "Host online"
                : "Host offline"}
        </span>
      </div>
      {!pinned && (
        <p
          role="alert"
          className="mt-2 text-xs text-red-600"
          data-testid="host-repair"
        >
          This host was paired before its identity key was pinned, so nothing it
          says can be verified. Forget it and pair again with the descriptor
          from <code>host credential descriptor</code>.
        </p>
      )}
      <label className="mt-2 block text-sm">
        Authorization
        <select
          aria-label={`Authorization for ${label}`}
          className="ml-2 rounded border bg-background p-1"
          value={authMode}
          onChange={(e) => {
            closePrivateChannels();
            setHostAuthMode(id, e.target.value as "passkey" | "browser-key");
          }}
        >
          <option value="passkey">Passkey delegation</option>
          <option value="browser-key">Locally trusted browser key</option>
        </select>
      </label>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <span className="text-xs text-muted-foreground" data-testid="host-lock">
          {authMode === "browser-key"
            ? "The host must have approved this browser key locally. Its grants determine access."
            : delegation.data
              ? `Unlocked until ${formatTime(delegation.data.notAfter)}`
              : "Locked — a passkey approval is needed to start or reopen sessions"}
        </span>
        {authMode === "passkey" && (
          <Button
            size="sm"
            onClick={() => void unlock()}
            disabled={busy || !pinned}
          >
            {delegation.data ? "Renew with passkey" : "Unlock with passkey"}
          </Button>
        )}
        {authMode === "passkey" && delegation.data && (
          <Button size="sm" variant="outline" onClick={() => void lock()}>
            Lock now
          </Button>
        )}
        <Button
          size="sm"
          variant="ghost"
          onClick={() => {
            unpairHost(id);
            void forgetDelegation(id);
          }}
        >
          Forget host
        </Button>
      </div>
    </li>
  );
}

/** Settings section: which hosts this browser trusts, and unlocking them. */
export function HostPairing() {
  const { hosts } = usePairedHosts();
  const { toast } = useToast();
  const [hostId, setHostId] = React.useState("");
  const [label, setLabel] = React.useState("");
  const [authMode, setAuthMode] = React.useState<"passkey" | "browser-key">(
    "passkey",
  );

  const pair = async (event: React.FormEvent) => {
    event.preventDefault();
    try {
      closePrivateChannels();
      await pairHost(hostId, label, authMode);
      setHostId("");
      setLabel("");
    } catch (error) {
      toast((error as Error).message, "error");
    }
  };

  return (
    <section aria-labelledby="hosts-heading" className="space-y-3" id="hosts">
      <h2 id="hosts-heading" className="text-lg font-semibold">
        Hosts
      </h2>
      <p className="text-sm text-muted-foreground">
        Starting and reopening sessions is signed by this browser and checked by
        your own machine. Your account login is not enough, and the server
        cannot do it for you. Pair a host with the descriptor it prints (it
        carries the host's public key, which this browser pins and checks every
        answer against). Use a passkey (valid for 24 hours), or select a browser
        key that the owner has approved locally.
      </p>
      {hosts.length === 0 ? (
        <p className="text-sm" data-testid="no-hosts">
          No host is paired. Starting and reopening sessions is off until you
          pair one.
        </p>
      ) : (
        <ul className="space-y-2">
          {hosts.map((host) => (
            <HostRow
              key={host.id}
              id={host.id}
              label={host.label}
              pinned={isPinned(host)}
              authMode={host.authMode ?? "passkey"}
            />
          ))}
        </ul>
      )}
      {hosts.some(isPinned) && <HostSharing />}
      <form
        onSubmit={(e) => void pair(e)}
        className="space-y-2 rounded-md border border-dashed border-border p-3"
      >
        <label className="block text-sm">
          Host descriptor
          <Input
            value={hostId}
            onChange={(e) => setHostId(e.target.value)}
            placeholder="chd1.… from `host credential descriptor`"
            className="font-mono text-xs"
          />
        </label>
        <label className="block text-sm">
          Name
          <Input
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            placeholder="My laptop"
          />
        </label>
        <label className="block text-sm">
          Authorization
          <select
            className="ml-2 rounded border bg-background p-1"
            value={authMode}
            onChange={(e) =>
              setAuthMode(e.target.value as "passkey" | "browser-key")
            }
          >
            <option value="passkey">Passkey delegation</option>
            <option value="browser-key">Locally trusted browser key</option>
          </select>
        </label>
        <Button type="submit" size="sm" disabled={hostId.trim() === ""}>
          Pair host
        </Button>
      </form>
      <details className="text-xs text-muted-foreground">
        <summary className="cursor-pointer">
          First-time setup on the host
        </summary>
        <ol className="mt-2 list-decimal space-y-1 pl-5">
          <li>
            Run <code>arrakis-conversations host credential descriptor</code> to
            print the host descriptor, and set <code>webauthn.rp_id</code> and{" "}
            <code>webauthn.origins</code> in the host settings to include this
            page ({window.location.origin}).
          </li>
          <li>
            Run <code>arrakis-conversations host run</code>, then{" "}
            <code>
              arrakis-conversations host passkey enroll-start --label laptop
              --grant session.start --grant session.reopen --grant history.read
              --grant session.write --grant session.pr_comment --grant
              session.manage_sharing --grant host.settings_read --grant
              host.settings_write --grant passkey.manage
            </code>{" "}
            and open the link it prints in this browser. The last five grants
            let this browser drive PR comments, session sharing, key approval
            and the host launch settings — without them, those actions fail with
            "operation outside the delegation's scope". PR refresh follows{" "}
            <code>session.write</code>, so the historical four-cap set already
            authorizes it. An owner credential approved without the extra caps
            can be upgraded in place from the local management page; see
            Settings → Access to this host.
          </li>
          <li>
            Approve the registration on the host:{" "}
            <code>host passkey pending</code>, then{" "}
            <code>host passkey approve &lt;id&gt;</code>. This page cannot
            approve a passkey.
          </li>
        </ol>
      </details>
    </section>
  );
}
