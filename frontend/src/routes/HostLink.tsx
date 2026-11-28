import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { fromBase64Url } from "@/lib/hostAuth";
import {
  closePrivateChannels,
  finishEnrollment,
  unlockHost,
  passkeyVouch,
} from "@/lib/hostControl";
import {
  pairedHost,
  activeHost,
  pairHost,
  isPinned,
  setHostAuthMode,
} from "@/lib/hostPairing";
import { parseDescriptor, type PinnedHost } from "@/lib/hostStatements";
import { pairCode } from "@/lib/pairCode";
import { useLocalHost } from "@/lib/localHost";

interface Ceremony {
  host: PinnedHost;
  descriptor: string;
  challenge: string;
  options: unknown;
}

/**
 * Parse the fragment written by `passkey enroll-start`. The link carries
 * the host's public key; the id shown and pinned is derived from it. Nothing
 * leaves the page.
 */
export async function parseHostLink(hash: string): Promise<Ceremony | null> {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  const descriptor = params.get("desc") ?? "";
  const challenge = params.get("challenge") ?? "";
  const encoded = params.get("options") ?? "";
  if (!/^[0-9a-f]{32}$/.test(challenge) || !encoded) return null;
  try {
    const host = await parseDescriptor(descriptor);
    const options = JSON.parse(
      new TextDecoder().decode(fromBase64Url(encoded)),
    ) as unknown;
    return { host, descriptor, challenge, options };
  } catch {
    return null;
  }
}

/**
 * Landing page for the link printed by `passkey enroll-start`. The host
 * started this registration locally; this page only runs the passkey prompt and
 * hands the result back. The registration is NOT trusted until it is approved
 * on the host itself.
 */
export function HostLink() {
  const navigate = useNavigate();
  const cache = useQueryClient();
  const local = useLocalHost();
  const [ceremony, setCeremony] = React.useState<Ceremony | null | undefined>(
    undefined,
  );
  React.useEffect(() => {
    void parseHostLink(window.location.hash).then(setCeremony);
  }, []);
  const [state, setState] = React.useState<
    | { kind: "idle" }
    | { kind: "working" }
    | { kind: "done"; credId: string; code: string | null }
    | { kind: "error"; message: string }
  >({ kind: "idle" });
  const [confirmed, setConfirmed] = React.useState(false);
  const [registered, setRegistered] = React.useState(false);
  const [credentialId, setCredentialId] = React.useState("");
  const [code, setCode] = React.useState<string | null>(null);

  if (local.local) {
    return (
      <p role="alert">
        Open this invite on your hosted Conversations server. Device
        registration is not available on the local host page.
      </p>
    );
  }
  if (ceremony === undefined) return null;
  if (!ceremony) {
    return (
      <div
        className="mx-auto max-w-xl space-y-2 p-4"
        data-testid="host-link-invalid"
      >
        <h1 className="text-xl font-semibold">Passkey link not valid</h1>
        <p className="text-sm text-muted-foreground">
          Open Settings on your laptop and choose Add a device to get a new
          link.
        </p>
      </div>
    );
  }

  const existing = pairedHost(ceremony.host.id);
  const alreadyPaired = existing !== null && isPinned(existing);

  const register = async () => {
    setState({ kind: "working" });
    try {
      // If this browser has a DIFFERENT host paired and active (a colleague
      // completing an invite), that host vouches for this ceremony (EX-04):
      // the vouch is signed AFTER the passkey exists — binding exactly that
      // key — and rides the enrollment. Both sides then derive the same
      // pairwise code from the two host keys. Pairing the ceremony host
      // itself (the owner) needs no cross-host anchor; no other host → no
      // vouch either: the offer still lands, and the owner's side demands a
      // stronger acknowledgement instead. A colleague's failed vouch aborts
      // the offer visibly (no silent downgrade): nothing is submitted.
      const own = activeHost();
      const crossHost = own !== null && own.id !== ceremony.host.id;
      const credId = await finishEnrollment(
        ceremony.host,
        ceremony.challenge,
        ceremony.options,
        crossHost && own
          ? async (id) => {
              const proof = await passkeyVouch(
                ceremony.host.id,
                ceremony.challenge,
                id,
                own.id,
              );
              if (proof.host_key !== own.key)
                throw new Error(
                  "The vouch came from a different host. Reopen the invite and try again.",
                );
              return proof;
            }
          : undefined,
      );
      if (!alreadyPaired) await pairHost(ceremony.descriptor, "My host");

      setCredentialId(credId);
      setRegistered(true);
      const code =
        crossHost && own?.key
          ? await pairCode(ceremony.host.key, own.key)
          : null;
      setCode(code);
      setState({ kind: "done", credId, code });
    } catch (error) {
      setState({ kind: "error", message: (error as Error).message });
    }
  };

  const continueToSessions = async () => {
    setState({ kind: "working" });
    try {
      if (!alreadyPaired) await pairHost(ceremony.descriptor, "My host");
      await unlockHost(ceremony.host.id);
      setHostAuthMode(ceremony.host.id, "passkey");
      closePrivateChannels();
      await cache.invalidateQueries({
        queryKey: ["host-delegation", ceremony.host.id],
      });
      navigate("/");
    } catch (error) {
      setState({ kind: "error", message: (error as Error).message });
    }
  };

  return (
    <div className="mx-auto max-w-xl space-y-3 p-4">
      <h1 className="text-xl font-semibold">Connect this device</h1>
      <p className="text-sm text-muted-foreground">
        Your laptop started this request. Only continue if you opened the link
        from your own laptop.
      </p>
      <details className="text-xs text-muted-foreground">
        <summary>Connection details</summary>
        <code
          className="block break-all rounded-md bg-muted p-2"
          data-testid="link-host-id"
        >
          {ceremony.host.id}
        </code>
      </details>
      {!registered && !alreadyPaired && (
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={confirmed}
            onChange={(e) => setConfirmed(e.target.checked)}
          />
          I opened this setup link from my own laptop.
        </label>
      )}
      {!registered && (
        <div className="flex flex-wrap gap-2">
          <Button
            onClick={() => void register()}
            disabled={state.kind === "working" || (!alreadyPaired && !confirmed)}
          >
            {state.kind === "working" ? "Waiting for passkey…" : "Create passkey"}
          </Button>
          <Button
            variant="outline"
            onClick={() => void continueToSessions()}
            disabled={state.kind === "working" || (!alreadyPaired && !confirmed)}
          >
            Use approved passkey
          </Button>
        </div>
      )}
      {state.kind === "error" && (
        <p role="alert" className="text-sm text-red-600">
          {state.message}
        </p>
      )}
      {registered && (
        <div
          className="space-y-2 rounded-md border border-border p-3 text-sm"
          data-testid="host-link-done"
        >
          <p>
            Your passkey is registered but <strong>not trusted yet</strong>.
            Approve this device on your laptop, then continue here.
          </p>
          {code && (
            <p>
              Your pair code:{" "}
              <code
                data-testid="pair-code"
                className="rounded bg-muted px-2 py-1 font-medium"
              >
                {code}
              </code>{" "}
              — read it to the host&rsquo;s owner and compare with theirs once;
              matching words prove this offer is really yours.
            </p>
          )}
          <p>Match this code on your laptop before approving:</p>
          <code
            className="block break-all rounded bg-background p-3 text-lg font-semibold tracking-wider"
            aria-label="Device verification code"
          >
            {credentialId.slice(-12)}
          </code>
          <details>
            <summary>Full device identifier</summary>
            <code className="break-all text-xs">{credentialId}</code>
          </details>
          <Button
            disabled={state.kind === "working"}
            onClick={() => void continueToSessions()}
          >
            Continue to sessions
          </Button>
          <p className="text-muted-foreground">
            You may be asked to use your passkey after your laptop approves this
            device.
          </p>
        </div>
      )}
    </div>
  );
}
