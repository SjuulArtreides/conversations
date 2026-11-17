import { useQuery, useQueryClient } from "@tanstack/react-query";
import { usePairedHosts } from "@/lib/hostPairing";
import { LOCAL_HOST_ID, useLocalHost } from "@/lib/localHost";
import * as React from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useToast } from "@/components/ui/toast";
import { hostControl, HostControlError } from "@/lib/hostControl";

const SESSION_ID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const CREDENTIAL_ID = /^[A-Za-z0-9_-]{43}$/;

/**
 * Let a credential your host already approved read or write ONE of your sessions,
 * or take that back. Signed with this browser's unlocked key and decided by
 * the host: it checks you may manage THAT session and that the credential's
 * locally approved ceiling covers it. This form cannot add credentials or
 * raise a ceiling, and it is not publication: nothing here reaches the server's
 * published copy or its readers.
 */
export function HostSharing({
  session: fixed,
  hostId,
}: {
  session?: string;
  /** The host that holds the session; the active host when omitted. */
  hostId?: string;
} = {}) {
  const { toast } = useToast();
  const queryClient = useQueryClient();
  const { active } = usePairedHosts();
  const local = useLocalHost();
  const selectedHost = hostId ?? (local.local ? LOCAL_HOST_ID : active);
  const credentials = useQuery({
    queryKey: ["host-access", selectedHost],
    queryFn: () => hostControl.listAccess(selectedHost ?? undefined),
    retry: false,
    staleTime: 30_000,
  });
  const manual =
    credentials.error instanceof HostControlError &&
    credentials.error.code === "refused" &&
    /owner|grant|authoriz|permission|delegation/i.test(
      credentials.error.message,
    );

  const [typed, setSession] = React.useState("");
  const session = fixed ?? typed;
  const [recipient, setRecipient] = React.useState("");
  const [read, setRead] = React.useState(true);
  const [write, setWrite] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  React.useEffect(() => {
    setRecipient("");
    setRead(true);
    setWrite(false);
  }, [selectedHost]);
  const selected = credentials.data?.find(
    (credential) => credential.kid === recipient,
  );
  const fingerprint = selected?.kid ?? "";
  const current =
    selected?.grants.filter(
      (grant) => grant.session === session.trim() || grant.session === "*",
    ) ?? [];

  const valid =
    SESSION_ID.test(session.trim()) &&
    CREDENTIAL_ID.test(recipient.trim()) &&
    (manual || (!!selected && !selected.revoked)) &&
    (read || write);
  const caps = [
    ...(read ? ["history.read"] : []),
    ...(write ? ["session.write"] : []),
  ];
  const access = read && write ? "read and write" : read ? "read" : "write";

  const change = async (remove: boolean) => {
    if (!valid || busy) return;
    setBusy(true);
    try {
      if (remove) {
        await hostControl.removeGrant(
          session.trim(),
          recipient.trim(),
          caps,
          hostId,
        );
        toast("Removed the selected session grants.");
      } else {
        await hostControl.addGrant(
          session.trim(),
          recipient.trim(),
          caps,
          hostId,
        );
        toast(`That credential can now ${access} this session, privately.`);
      }
      await queryClient.invalidateQueries({
        queryKey: ["host-access", selectedHost],
      });
      setRecipient("");
    } catch (error) {
      toast((error as Error).message, "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void change(false);
      }}
      className="space-y-2 rounded-md border border-dashed border-border p-3"
      aria-label="Share a session"
    >
      <p className="text-sm font-medium">
        Allow a credential to access a session
      </p>
      <p className="text-xs text-muted-foreground">
        For keys your host already approved. They act on the session through
        your host, privately; this does not publish anything to the server.
      </p>
      {fixed === undefined && (
        <label className="block text-sm">
          Session id
          <Input
            value={typed}
            onChange={(e) => setSession(e.target.value)}
            className="font-mono text-xs"
            placeholder="the session's UUID"
          />
        </label>
      )}
      {credentials.isPending && (
        <p role="status" className="text-sm text-muted-foreground">
          Loading approved keys…
        </p>
      )}
      {credentials.data && (
        <>
          <label className="block text-sm">
            Approved key
            <select
              aria-label="Approved key"
              value={recipient}
              disabled={busy}
              onChange={(event) => setRecipient(event.target.value)}
              className="mt-1 block h-9 w-full rounded-md border border-input bg-card px-3 text-sm"
            >
              <option value="">Choose an approved key</option>
              {credentials.data.map((credential) => (
                <option
                  key={credential.kid}
                  value={credential.kid}
                  disabled={credential.revoked}
                >
                  {credential.label || credential.kid}
                  {credential.revoked ? " (revoked)" : ""}
                </option>
              ))}
            </select>
          </label>
          {!credentials.data.length && (
            <p className="text-sm text-muted-foreground">
              No keys are enrolled on this host.
            </p>
          )}
        </>
      )}
      {manual && (
        <>
          <p className="text-sm text-muted-foreground">
            Your host does not allow this key to list approved credentials.
            Enter an already-approved credential id; the host still checks the
            selected permissions.
          </p>
          <label className="block text-sm">
            Credential id
            <Input
              value={recipient}
              onChange={(event) => setRecipient(event.target.value)}
              className="font-mono text-xs"
              placeholder="43-character id of a key already approved by your host"
            />
          </label>
        </>
      )}
      {credentials.error && !manual && (
        <p role="alert" className="text-sm text-destructive">
          Could not load approved keys: {credentials.error.message}{" "}
          <button
            type="button"
            className="underline"
            onClick={() => void credentials.refetch()}
          >
            Retry
          </button>
        </p>
      )}
      {selected && (
        <div className="space-y-1 text-xs text-muted-foreground">
          <p>Full fingerprint</p>
          <output
            aria-label="Credential fingerprint"
            className="block break-all font-mono"
          >
            {fingerprint}
          </output>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() =>
              void navigator.clipboard
                .writeText(fingerprint)
                .then(() => toast("Fingerprint copied."))
                .catch(() => toast("Could not copy fingerprint.", "error"))
            }
          >
            Copy fingerprint
          </Button>
          <p>
            Current access:{" "}
            {current.some((grant) => grant.cap === "history.read")
              ? "read history"
              : "no history access"}
            ;{" "}
            {current.some((grant) => grant.cap === "session.write")
              ? "write"
              : "no write access"}
            .
            {current.some((grant) => grant.session === "*")
              ? " Host-wide grants also apply."
              : ""}
          </p>
        </div>
      )}
      <label className="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={read}
          onChange={(e) => setRead(e.target.checked)}
        />
        Read this session’s history
      </label>
      <label className="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={write}
          onChange={(event) => setWrite(event.target.checked)}
        />
        Write to this session
      </label>
      <div className="flex flex-wrap gap-2">
        <Button type="submit" size="sm" disabled={!valid || busy}>
          {busy ? "Asking your host…" : "Allow selected access"}
        </Button>
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={!valid || busy}
          onClick={() => void change(true)}
        >
          Remove access
        </Button>
      </div>
    </form>
  );
}
