import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import type { Visibility } from "@/lib/types";
import {
  hostControl,
  HostControlError,
  type PasskeyOffer,
  type PublicationState,
  type PublicationStatus,
} from "@/lib/hostControl";
import { usePairedHosts } from "@/lib/hostPairing";
import { pairCode } from "@/lib/pairCode";
import { LOCAL_HOST_ID, localRequest, useLocalHost } from "@/lib/localHost";
import { parseDescriptor } from "@/lib/hostStatements";
import { HostSharing } from "./HostSharing";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Link } from "react-router-dom";
import { RadioGroup, RadioGroupItem } from "./ui/radio-group";
import { Button } from "./ui/button";
import { EmailAutocomplete } from "./EmailAutocomplete";

const STATE_LABEL: Record<PublicationState, string> = {
  private: "Private",
  syncing: "Syncing",
  shared: "Shared",
  deletion_pending: "Deletion pending",
  error: "Error",
};

/** What the sharer copies for a colleague: the invite link with enough
 * context to act on it without asking back. */
export function inviteMessage(title: string, link: string): string {
  return `I'd like to let you type into my Conversations session "${title}". Open this link and create a passkey when it asks. I'll then confirm with you that the key is really yours before I trust it.\n\n${link}`;
}

function hostProblem(err: unknown): string {
  if (err instanceof HostControlError && err.code === "offline") {
    return "The host is offline. The published copy stays as it is until the host is back.";
  }
  return err instanceof Error ? err.message : String(err);
}

interface SessionShareDialogProps {
  /** The paired host that holds the session; it alone publishes and grants. */
  hostId: string;
  sessionId: string;
  title: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * One Share dialog for a session on a paired host.
 *
 * Read = the host publishes a read-only copy for the chosen people: they read
 * it on the server with their login, even while the host is offline, and it
 * never lets them type. Typing stays blocked until a key is trusted — the
 * sharer is told so at share time.
 *
 * Write = keys the host already approved, granted per session through the
 * private channel. Both changes are signed operations to that host; the
 * server relays and stores only the published read-only copy.
 *
 * The form starts from the host's signed status, never from a default: until
 * the host has said who reads the session, nothing can be saved over it.
 */
export function SessionShareDialog({
  hostId,
  sessionId,
  title,
  open,
  onOpenChange,
}: SessionShareDialogProps) {
  const [visibility, setVisibility] = React.useState<Visibility>("restricted");
  const [emails, setEmails] = React.useState<string[]>([]);
  const [writePending, setWritePending] = React.useState<string[]>([]);
  const [status, setStatus] = React.useState<PublicationStatus | null>(null);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [notice, setNote] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  // Key-trust ceremony state (EX-03/EX-04): an invite link to hand out, and
  // the pending offers this host is holding.
  const [invite, setInvite] = React.useState<{
    email: string;
    link: string;
  } | null>(null);
  const [inviting, setInviting] = React.useState<string | null>(null);
  const [copied, setCopied] = React.useState(false);
  const [granting, setGranting] = React.useState<string | null>(null);
  const [offers, setOffers] = React.useState<PasskeyOffer[] | null>(null);
  const [approving, setApproving] = React.useState<string | null>(null);
  /** Per-offer confirmations: the pair code was compared (or another
   * verification was made) before "Trust this key" unlocks (EX-04/U-7). */
  const [confirmed, setConfirmed] = React.useState<Record<string, boolean>>({});
  /** Pairwise codes for the current offers, by credential id (EX-04). */
  const [codes, setCodes] = React.useState<Record<string, string>>({});
  const queryClient = useQueryClient();
  const reading = React.useRef(0);
  const readingOffers = React.useRef(0);
  // This browser's pinned key for the host whose offers these are: one side
  // of every pairwise code.
  const pairedKey = usePairedHosts().hosts.find((h) => h.id === hostId)?.key;
  const local = useLocalHost();
  const localKey = useQuery({
    queryKey: ["local-host-pair-code-key"],
    queryFn: async () => {
      const status = await localRequest<{ descriptor: string }>("/api/status");
      return (await parseDescriptor(status.descriptor)).key;
    },
    enabled:
      open && hostId === LOCAL_HOST_ID && local.local && local.authenticated,
  });
  const myHostKey = hostId === LOCAL_HOST_ID ? localKey.data : pairedKey;
  // The keys this host already trusts: an invite labels its key with the
  // invitee's email, so a person's row can say whether they can type yet.
  const access = useQuery({
    queryKey: ["host-access", hostId],
    queryFn: () => hostControl.listAccess(hostId),
    enabled: open,
    retry: false,
    staleTime: 30_000,
  });

  const readStatus = React.useCallback(async () => {
    const read = (reading.current += 1);
    try {
      const next = await hostControl.publicationStatus(hostId, sessionId);
      if (read !== reading.current) return;
      setStatus(next);
      setVisibility(next.audience?.visibility ?? "restricted");
      setEmails(
        next.audience?.visibility === "restricted" ? next.audience.emails : [],
      );
      setWritePending(
        next.audience?.visibility === "restricted"
          ? (next.audience.write_pending ?? [])
          : [],
      );
    } catch (err) {
      if (read === reading.current) setProblem(hostProblem(err));
    }
  }, [hostId, sessionId]);

  const readOffers = React.useCallback(async () => {
    const read = (readingOffers.current += 1);
    try {
      const next = await hostControl.passkeyPending(hostId);
      if (read !== readingOffers.current) return;
      setOffers(next);
    } catch {
      // Not authorized for the ceremony (an older owner passkey lacks the
      // cap): the section stays silent; the host's refusal is shown only
      // when the owner acts.
      if (read === readingOffers.current) setOffers(null);
    }
  }, [hostId]);

  // Derive the pairwise code for every vouched offer (EX-04): this host's
  // pinned key and the vouching host's key, both public. The colleague's own
  // UI derives the same words from the same two keys.
  React.useEffect(() => {
    if (!myHostKey || !offers) {
      setCodes({});
      return;
    }
    let stopped = false;
    void (async () => {
      const next: Record<string, string> = {};
      for (const offer of offers) {
        if (offer.vouch_host_key) {
          next[offer.cred_id] = await pairCode(myHostKey, offer.vouch_host_key);
        }
      }
      if (!stopped) setCodes(next);
    })();
    return () => {
      stopped = true;
    };
  }, [myHostKey, offers]);

  React.useEffect(() => {
    if (!open) return;
    setStatus(null);
    setVisibility("restricted");
    setEmails([]);
    setWritePending([]);
    setProblem(null);
    setNote(null);
    setInvite(null);
    setCopied(false);
    setOffers(null);
    setConfirmed({});
    void readStatus();
    void readOffers();
    return () => {
      reading.current += 1;
      readingOffers.current += 1;
    };
  }, [open, readStatus, readOffers]);

  const run = async (
    change: () => Promise<PublicationState>,
    note?: string,
  ) => {
    setBusy(true);
    setProblem(null);
    try {
      await change();
      queryClient.invalidateQueries({ queryKey: ["session", sessionId] });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      // Show what the host now holds, not what was asked for.
      setStatus(null);
      await readStatus();
      // A toast would land in the aria-hidden app background while this modal
      // is open: the notice belongs in the dialog, where the user is looking.
      setNote(note ?? null);
    } catch (err) {
      setProblem(hostProblem(err));
    } finally {
      setBusy(false);
    }
  };

  const state = status?.state ?? null;
  const unpublished = state === "private";
  // Only a colleague's (agent) key can be trusted from here; the host refuses
  // owner keys over a channel. Your own devices are approved on the host.
  const agentOffers = (offers ?? []).filter((o) => o.kind === "agent");
  const ownerOffers = (offers ?? []).filter((o) => o.kind !== "agent");

  /** Create the enrollment invite for one pending writer (EX-03): an agent
   * key scoped to exactly this session — it may type here and read this
   * history, nothing else — once the owner trusts it (EX-04). */
  const sendInvite = async (email: string) => {
    if (inviting) return;
    setInviting(email);
    setProblem(null);
    setCopied(false);
    // Inviting someone to type is the intent to let them write: they stay
    // read-only until their key is trusted (EX-02).
    setWritePending((prev) => (prev.includes(email) ? prev : [...prev, email]));
    try {
      const link = await hostControl.passkeyInvite(hostId, email, [
        `session.write:${sessionId}`,
        `history.read:${sessionId}`,
      ]);
      setInvite({ email, link });
    } catch (err) {
      setProblem(hostProblem(err));
    } finally {
      setInviting(null);
    }
  };

  const copyInvite = async () => {
    if (!invite) return;
    try {
      await navigator.clipboard.writeText(inviteMessage(title, invite.link));
      setCopied(true);
    } catch {
      setProblem(
        "Could not copy to the clipboard. Select the text and copy it.",
      );
    }
  };

  React.useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 2500);
    return () => clearTimeout(timer);
  }, [copied]);

  /** Their key is already trusted here for another session: grant this one. */
  const allowTyping = async (email: string, kid: string) => {
    if (granting) return;
    setGranting(email);
    setProblem(null);
    try {
      await hostControl.addGrant(
        sessionId,
        kid,
        ["history.read", "session.write"],
        hostId,
      );
      await queryClient.invalidateQueries({
        queryKey: ["host-access", hostId],
      });
      setNote(`${email} can now type into this session.`);
    } catch (err) {
      setProblem(hostProblem(err));
    } finally {
      setGranting(null);
    }
  };

  /** Where one person stands on typing, from the keys this host holds. */
  const typingState = (email: string) => {
    // Only colleague (agent) keys: an owner device is never a person here.
    const keys = (access.data ?? []).filter(
      (c) =>
        c.kind === "agent" &&
        !c.revoked &&
        c.label.trim().toLowerCase() === email,
    );
    if (
      keys.some((c) =>
        c.grants.some(
          (g) =>
            g.cap === "session.write" &&
            (g.session === sessionId || g.session === "*"),
        ),
      )
    )
      return { kind: "can-type" as const };
    // A label is a name, not an identity: grant from here only when it names
    // exactly one key, and show which.
    if (keys.length === 1)
      return { kind: "trusted" as const, kid: keys[0].kid };
    if (keys.length > 1) return { kind: "ambiguous" as const };
    if (agentOffers.some((o) => o.label.trim().toLowerCase() === email))
      return { kind: "offered" as const };
    return { kind: "untrusted" as const };
  };

  /** Pick up keys that arrived since the dialog opened: new offers to
   * confirm, and keys trusted elsewhere in the meantime. */
  const refreshKeys = async () => {
    await Promise.all([
      readOffers(),
      queryClient.invalidateQueries({ queryKey: ["host-access", hostId] }),
    ]);
  };

  /** Approve one offered key: the HOST records the trust locally (PRI-001);
   * this browser only carried the owner's decision to it. */
  const approveOffer = async (credId: string) => {
    if (approving) return;
    setApproving(credId);
    setProblem(null);
    try {
      await hostControl.passkeyApprove(hostId, credId);
      setNote(
        "Key trusted: it can now read and type into the sessions it was invited to.",
      );
      await Promise.all([
        readOffers(),
        queryClient.invalidateQueries({ queryKey: ["host-access", hostId] }),
      ]);
    } catch (err) {
      setProblem(hostProblem(err));
    } finally {
      setApproving(null);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        aria-describedby="share-desc"
        className="max-h-[90dvh] overflow-y-auto"
      >
        <DialogHeader>
          <DialogTitle>Share session</DialogTitle>
          <DialogDescription id="share-desc">
            Choose who can read “{title}” and who can type into it. Reading
            shares a read-only copy on the server — readable while your host is
            offline — and never lets anyone type. Typing needs a key your host
            approved.
          </DialogDescription>
        </DialogHeader>

        {status?.error && (
          <p className="text-sm text-destructive">{status.error}</p>
        )}
        <RadioGroup
          value={visibility}
          onValueChange={(v) => setVisibility(v as Visibility)}
          aria-label="Visibility"
          disabled={!status}
        >
          <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-border p-3">
            <RadioGroupItem
              value="organization"
              id="share-org"
              aria-label="Entire organization"
            />
            <span className="text-sm font-medium">🌐 Entire organization</span>
          </label>
          <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-border p-3">
            <RadioGroupItem
              value="restricted"
              id="share-restricted"
              aria-label="Specific people"
            />
            <span className="text-sm font-medium">🔒 Specific people</span>
          </label>
        </RadioGroup>
        {status && visibility === "restricted" && (
          <EmailAutocomplete
            selected={emails}
            onAdd={(e) => setEmails((prev) => [...prev, e])}
            onRemove={(e) => {
              setEmails((prev) => prev.filter((x) => x !== e));
              setWritePending((prev) => prev.filter((x) => x !== e));
            }}
          />
        )}
        {status && visibility === "restricted" && emails.length > 0 && (
          <ul className="space-y-2" aria-label="People">
            {emails.map((email) => {
              const typing = typingState(email);
              return (
                <li
                  key={email}
                  className="flex flex-wrap items-center gap-2 rounded-md border border-border p-2"
                  data-testid={`person-${email}`}
                >
                  <span className="min-w-0 flex-1 break-all text-sm">
                    {email}
                  </span>
                  {typing.kind === "can-type" ? (
                    <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-xs text-emerald-700 dark:text-emerald-300">
                      Can type
                    </span>
                  ) : typing.kind === "offered" ? (
                    <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-xs text-amber-700 dark:text-amber-300">
                      Key offered · confirm it below
                    </span>
                  ) : (
                    <>
                      <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-xs text-amber-700 dark:text-amber-300">
                        {typing.kind === "trusted"
                          ? `Read-only here · key ${typing.kid.slice(0, 8)}…`
                          : typing.kind === "ambiguous"
                            ? "Several keys carry this name · grant one under Advanced"
                            : "Read-only · key not trusted"}
                      </span>
                      {typing.kind === "ambiguous" ? null : typing.kind ===
                        "trusted" ? (
                        <Button
                          size="sm"
                          disabled={granting !== null}
                          onClick={() => void allowTyping(email, typing.kid)}
                          title={`Grant typing to key ${typing.kid}`}
                        >
                          {granting === email ? "Allowing…" : "Allow typing"}
                        </Button>
                      ) : (
                        <Button
                          size="sm"
                          disabled={inviting !== null}
                          onClick={() => void sendInvite(email)}
                          aria-label={`Invite ${email} to type`}
                        >
                          {inviting === email
                            ? "Creating invite…"
                            : "Invite to type"}
                        </Button>
                      )}
                    </>
                  )}
                </li>
              );
            })}
          </ul>
        )}
        {status &&
          visibility === "restricted" &&
          !invite &&
          emails.some((e) => typingState(e).kind !== "can-type") && (
            <Button
              size="sm"
              variant="ghost"
              className="self-start"
              onClick={() => void refreshKeys()}
            >
              Check for new keys
            </Button>
          )}

        {invite && (
          <div
            className="space-y-2 rounded-md border border-primary/40 bg-primary/5 p-3"
            aria-label="Key invite"
          >
            <p className="text-sm font-medium">
              Send this invite to {invite.email}
            </p>
            <p className="text-xs text-muted-foreground">
              Over any channel you would talk to them on. When they create their
              passkey, their key shows up here for you to confirm.
            </p>
            <textarea
              readOnly
              rows={4}
              value={inviteMessage(title, invite.link)}
              aria-label="Invite message"
              className="w-full rounded-md border border-input bg-background p-2 text-xs"
            />
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" onClick={() => void copyInvite()}>
                {copied ? "Copied ✓" : "Copy invite"}
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => void refreshKeys()}
              >
                Check for their key
              </Button>
              {copied && (
                <span className="text-xs text-muted-foreground">
                  Invite message and link copied to your clipboard.
                </span>
              )}
            </div>
          </div>
        )}

        {agentOffers.length > 0 && (
          <div
            className="rounded-md border border-border p-3"
            aria-label="Key offers waiting for trust"
          >
            <p className="text-sm font-medium">
              Keys waiting for your approval
            </p>
            <ul className="mt-2 space-y-2">
              {agentOffers.map((offer) => {
                const code = codes[offer.cred_id];
                const canTrust =
                  offer.trusted_host ||
                  ((!offer.vouch_host_key || !!code) &&
                    !!confirmed[offer.cred_id]);
                return (
                  <li
                    key={offer.cred_id}
                    className="flex flex-col gap-1 rounded-md border border-border p-2"
                    data-testid={`offer-${offer.cred_id}`}
                  >
                    <div className="flex flex-wrap items-center gap-2 text-sm">
                      <span className="font-medium break-all">
                        {offer.label}
                      </span>
                      <code className="break-all rounded bg-muted px-1.5 py-0.5 text-xs">
                        {offer.cred_id.slice(0, 16)}…
                      </code>
                      <span className="text-xs text-muted-foreground">
                        {offer.kind} key ·{" "}
                        {offer.grants.join(", ") || "no grants"}
                      </span>
                    </div>
                    {code ? (
                      <p
                        data-testid={`pair-code-${offer.cred_id}`}
                        className="text-sm"
                      >
                        Pair code:{" "}
                        <span className="rounded bg-muted px-1.5 py-0.5 font-medium">
                          {code}
                        </span>{" "}
                        <span className="text-xs text-muted-foreground">
                          — read it to them and compare, once, over any channel
                          you would talk to them on.
                        </span>
                      </p>
                    ) : offer.vouch_host_key ? (
                      <p className="text-xs text-muted-foreground">
                        {localKey.isError
                          ? "Pair code unavailable. Reconnect to your host and refresh the offers."
                          : "Loading pair code…"}
                      </p>
                    ) : (
                      <p className="text-xs text-amber-600 dark:text-amber-400">
                        No pair code: their host did not vouch for this offer.
                        Verify the key another way before trusting it.
                      </p>
                    )}
                    {!offer.trusted_host && (
                      <label className="flex items-center gap-2 text-xs">
                        <input
                          type="checkbox"
                          checked={!!confirmed[offer.cred_id]}
                          disabled={!!offer.vouch_host_key && !code}
                          onChange={(event) =>
                            setConfirmed((prev) => ({
                              ...prev,
                              [offer.cred_id]: event.target.checked,
                            }))
                          }
                          aria-label={`Confirm this key: ${offer.label}`}
                        />
                        {offer.vouch_host_key
                          ? "We compared the pair code and it matched."
                          : "I verified this key is really theirs another way."}
                      </label>
                    )}
                    {offer.trusted_host && (
                      <p className="text-xs text-muted-foreground">
                        Their host already has a trusted key here: this offer
                        inherits that verification.
                      </p>
                    )}
                    <div>
                      <Button
                        size="sm"
                        disabled={approving !== null || !canTrust}
                        title={
                          canTrust
                            ? undefined
                            : "Confirm the pair code (or another verification) first"
                        }
                        onClick={() => void approveOffer(offer.cred_id)}
                      >
                        {approving === offer.cred_id
                          ? "Trusting…"
                          : "Trust this key"}
                      </Button>
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        )}

        {ownerOffers.length > 0 && (
          <p className="text-xs text-muted-foreground">
            {ownerOffers.length === 1
              ? `Your device “${ownerOffers[0].label}” is waiting for approval.`
              : `${ownerOffers.length} of your devices are waiting for approval.`}{" "}
            Approve your own devices in{" "}
            <Link to="/settings" className="underline">
              Settings → Access
            </Link>{" "}
            on the computer that runs your host.
          </p>
        )}

        {problem && (
          <p role="alert" className="text-sm text-destructive">
            {problem}
          </p>
        )}
        {notice && (
          <p role="status" className="text-sm text-muted-foreground">
            {notice}
          </p>
        )}

        <DialogFooter className="items-center">
          {state && state !== "private" && (
            <span className="mr-auto text-xs text-muted-foreground">
              <span data-publication-state>{STATE_LABEL[state]}</span>
              {status &&
                ` · ${status.published} published, ${status.pending} waiting`}
            </span>
          )}
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Close
          </Button>
          <Button
            variant="outline"
            disabled={busy || !status || unpublished}
            onClick={() =>
              run(() => hostControl.unsharePublication(hostId, sessionId))
            }
          >
            Stop sharing
          </Button>
          <Button
            disabled={busy || !status}
            onClick={() =>
              run(
                () =>
                  hostControl.sharePublication(
                    hostId,
                    sessionId,
                    visibility === "organization"
                      ? { visibility: "organization" }
                      : {
                          visibility: "restricted",
                          emails,
                          ...(writePending.length
                            ? { write_pending: writePending }
                            : {}),
                        },
                  ),
                writePending.length
                  ? "Shared: they can read now. The people you invited to type stay read-only until you trust their key."
                  : "Shared read-only: they can read it now, but can't type into it until you trust their key.",
              )
            }
          >
            {busy ? "Sending…" : unpublished ? "Share read-only" : "Save"}
          </Button>
        </DialogFooter>

        <details className="text-sm">
          <summary className="cursor-pointer text-xs text-muted-foreground">
            Advanced: grant an approved key by id
          </summary>
          <div className="mt-2">
            <HostSharing session={sessionId} hostId={hostId} />
          </div>
        </details>
      </DialogContent>
    </Dialog>
  );
}
