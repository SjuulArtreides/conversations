import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { Button, buttonVariants } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useBrowserPublication } from "@/lib/browserPublication";
import { inspectHostAccess } from "@/lib/hostSetup";
import { toBase64 } from "@/lib/hostAuth";
import {
  closePrivateChannels,
  HostControlError,
  hostOnline,
  unlockAgain,
} from "@/lib/hostControl";
import {
  activeHost,
  isPinned,
  pairHost,
  selectHost,
  usePairedHosts,
} from "@/lib/hostPairing";

function usePageVisible() {
  const [visible, setVisible] = React.useState(
    document.visibilityState !== "hidden",
  );
  React.useEffect(() => {
    const update = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", update);
    return () => document.removeEventListener("visibilitychange", update);
  }, []);
  return visible;
}

function Step({
  title,
  status,
  complete,
  current,
}: {
  title: string;
  status: string;
  complete: boolean;
  current: boolean;
}) {
  return (
    <li
      aria-current={current ? "step" : undefined}
      className={`flex items-start gap-3 rounded-lg border p-3 ${current && !complete ? "border-primary bg-primary/5" : "border-border/70"}`}
    >
      <span
        aria-hidden="true"
        className={complete ? "text-green-600" : "text-muted-foreground"}
      >
        {complete ? "✓" : current ? "●" : "○"}
      </span>
      <div>
        <div className="text-sm font-medium">{title}</div>
        <p className="mt-1 text-xs text-muted-foreground">{status}</p>
      </div>
    </li>
  );
}

const INSTALL_SCRIPT_URL =
  "https://raw.githubusercontent.com/SjuulArtreides/conversations/main/scripts/install.sh";

// Exactly what `scripts/install.sh --server` accepts: a browser-canonical
// origin (http(s), lowercase DNS name, IPv4 or bracketed IPv6, optional
// port). Every character it allows is inert in POSIX shells.
const CANONICAL_ORIGIN =
  /^https?:\/\/(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)*|\[[0-9a-f:.]+\])(?::[0-9]{1,5})?$/;

const IDENTITY_DENIED =
  /unknown, revoked or expired credential or delegation|does not have an approved owner credential|(?:credential|browser key|key) (?:is |was )?(?:unknown|revoked|expired|not approved)/i;

function canonicalWebOrigin(origin: string): string | null {
  return CANONICAL_ORIGIN.test(origin) ? origin : null;
}

export function HostSetup({
  active = true,
  webOrigin = window.location.origin,
}: {
  active?: boolean;
  /** The server's web origin; defaults to this page's. */
  webOrigin?: string;
}) {
  const navigate = useNavigate();
  const client = useQueryClient();
  const pairing = usePairedHosts();
  const host = pairing.hosts.find(
    (candidate) => candidate.id === pairing.active,
  );
  const mode = host?.authMode ?? "passkey";
  const visible = usePageVisible();
  const polling = active && visible && !!host && isPinned(host);
  const publication = useBrowserPublication();
  const [descriptor, setDescriptor] = React.useState("");
  const [machineName, setMachineName] = React.useState("");
  const [browserName, setBrowserName] = React.useState("My browser");
  const [publishedName, setPublishedName] = React.useState("My browser");
  const [nameUpdated, setNameUpdated] = React.useState(false);
  const [pairingError, setPairingError] = React.useState<string | null>(null);
  const [unlockError, setUnlockError] = React.useState<string | null>(null);
  const [unlocking, setUnlocking] = React.useState(false);
  const unlockAttempt = React.useRef(0);
  const [copyStatus, setCopyStatus] = React.useState<string | null>(null);
  const [installCopy, setInstallCopy] = React.useState<
    "copied" | "failed" | null
  >(null);
  const identity = `${host?.id ?? ""}:${mode}`;
  // Once the machine has confirmed this browser key is not approved, further
  // access handshakes cannot succeed until its owner acts, and each one holds
  // a private connection on the machine. Stop automatic access checks for
  // this host and mode until the user explicitly asks to check again.
  const [pausedIdentity, setPausedIdentity] = React.useState<string | null>(
    null,
  );
  const [pairingMachine, setPairingMachine] = React.useState(false);
  const awaitingApproval = pausedIdentity === identity;
  const [rechecking, setRechecking] = React.useState(false);
  const online = useQuery({
    queryKey: ["host-setup-status", host?.id, mode],
    queryFn: () => hostOnline(host!.id),
    enabled: polling,
    refetchInterval: polling ? 5000 : false,
    retry: false,
    gcTime: 0,
  });
  const accessPolling =
    polling &&
    online.isSuccess &&
    online.data === true &&
    !awaitingApproval &&
    !pairingMachine;
  const access = useQuery({
    queryKey: ["host-setup-access", host?.id, mode],
    queryFn: () => inspectHostAccess(host!.id),
    enabled: accessPolling,
    refetchInterval: accessPolling ? 5000 : false,
    retry: false,
    gcTime: 0,
  });
  const accessCode =
    access.error instanceof HostControlError ? access.error.code : null;
  const connected =
    online.isSuccess &&
    online.data === true &&
    (awaitingApproval || accessCode !== "offline");
  const verified = connected && access.isSuccess;
  const ready = verified && access.data.canRead && access.data.canStart;
  const previous = React.useRef({ identity: "", ready: false });
  React.useEffect(() => {
    if (
      ready &&
      (previous.current.identity !== identity || !previous.current.ready)
    ) {
      void client.invalidateQueries({ queryKey: ["private", host!.id] });
      void client.invalidateQueries({ queryKey: ["host-launch-settings"] });
    }
    previous.current = { identity, ready };
  }, [ready, identity, host, client]);
  React.useEffect(() => {
    unlockAttempt.current += 1;
    setUnlockError(null);
    setUnlocking(false);
    return () => {
      unlockAttempt.current += 1;
    };
  }, [identity]);

  const connect = async (event: React.FormEvent) => {
    event.preventDefault();
    setPairingError(null);
    setPairingMachine(true);
    try {
      closePrivateChannels();
      const paired = await pairHost(descriptor, machineName, "browser-key");
      setPausedIdentity(`${paired.id}:browser-key`);
      setDescriptor("");
      setMachineName("");
    } catch (error) {
      setPairingError((error as Error).message);
    } finally {
      setPairingMachine(false);
    }
  };
  const publishBrowser = (updating = false) => {
    const submittedName = browserName;
    publication.publish.mutate(submittedName, {
      onSuccess: () => {
        setPublishedName(submittedName);
        setNameUpdated(updating);
      },
    });
  };
  const unlock = async () => {
    if (!host) return;
    const attempt = ++unlockAttempt.current;
    const current = () => {
      const selected = activeHost();
      return (
        attempt === unlockAttempt.current &&
        selected?.id === host.id &&
        (selected.authMode ?? "passkey") === mode
      );
    };
    setUnlocking(true);
    setUnlockError(null);
    try {
      await unlockAgain(host.id);
      if (!current()) return;
      await client.invalidateQueries({
        queryKey: ["host-setup-access", host.id, mode],
      });
      await client.invalidateQueries({
        queryKey: ["host-delegation", host.id],
      });
    } catch (error) {
      if (current()) setUnlockError((error as Error).message);
    } finally {
      if (current()) setUnlocking(false);
    }
  };

  const checking =
    !!host &&
    isPinned(host) &&
    (online.isPending || (connected && access.isPending && !awaitingApproval));
  const offline =
    !!host &&
    ((online.isSuccess && online.data === false) ||
      (!awaitingApproval && accessCode === "offline"));
  const refusal = accessCode === "refused" ? (access.error?.message ?? "") : "";
  // The regular access poll retries temporary connection capacity refusals.
  const hostBusy =
    connected &&
    /too many private connection requests are open|too many private connections are open/.test(
      refusal,
    );
  const identityDenied = IDENTITY_DENIED.test(refusal);
  const approvalNeeded =
    connected &&
    (awaitingApproval ||
      (access.isError && (accessCode === "locked" || identityDenied)));
  // Passkey mode recovers through its own unlock flow, so only browser-key
  // approval pauses the access handshake.
  const keyNotApproved =
    mode === "browser-key" && connected && access.isError && identityDenied;
  React.useEffect(() => {
    if (keyNotApproved) setPausedIdentity(identity);
  }, [keyNotApproved, access.errorUpdatedAt, identity]);
  const recheckAttempt = React.useRef(0);
  React.useEffect(() => {
    recheckAttempt.current += 1;
    setRechecking(false);
  }, [identity]);
  const checkApproval = async () => {
    const attempt = ++recheckAttempt.current;
    const checked = identity;
    setRechecking(true);
    const result = await access.refetch();
    if (attempt !== recheckAttempt.current) return;
    setRechecking(false);
    if (result.isSuccess) {
      setPausedIdentity((paused) => (paused === checked ? null : paused));
    }
  };
  const refusalReview =
    connected && accessCode === "refused" && !identityDenied && !hostBusy;
  const insufficientPermissions =
    /no current grant|capability outside credential ceiling|operation outside.*delegation.*scope|only an owner credential may change session metadata/i.test(
      refusal,
    );
  const reviewNeeded = verified && !ready;
  const waiting =
    approvalNeeded &&
    mode === "browser-key" &&
    publication.registration.data === true;
  const problem =
    online.error ??
    (access.isError &&
    !approvalNeeded &&
    !refusalReview &&
    !hostBusy &&
    !offline
      ? access.error
      : null);
  const approvalCommand = publication.key.data
    ? [
        "conversations credential add --kind owner --alg es256",
        `--public-key ${toBase64(publication.key.data.publicRaw)}`,
        `--label '${browserName.replaceAll("'", "'\"'\"'")}'`,
        ...[
          "history.read",
          "session.start",
          "session.reopen",
          "session.write",
          "session.pr_comment",
          "session.manage_sharing",
          "host.settings_read",
          "host.settings_write",
          "passkey.manage",
        ].map((grant) => `--grant ${grant}`),
      ].join(" ")
    : "";
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopyStatus("Copied.");
    } catch {
      setCopyStatus("Could not copy. Select and copy the text directly.");
    }
  };
  const origin = canonicalWebOrigin(webOrigin);
  // The origin travels as a positional parameter of the `bash -c` wrapper,
  // never spliced into the script text, and is single-quoted even though a
  // canonical origin holds no shell-active characters.
  const installCommand = origin
    ? `bash -c 'f=$(mktemp) || exit; cleanup() { rm -f -- "$f"; }; trap cleanup EXIT; curl -fsSL --proto =https ${INSTALL_SCRIPT_URL} -o "$f" && bash "$f" --server "$1"' bash '${origin}'`
    : `bash -c 'f=$(mktemp) || exit; cleanup() { rm -f -- "$f"; }; trap cleanup EXIT; curl -fsSL --proto =https ${INSTALL_SCRIPT_URL} -o "$f" && bash "$f"'`;
  const loginCommand = origin
    ? `conversations login --server '${origin}'`
    : null;
  const hostCommand = origin ? `conversations host --server '${origin}'` : null;
  const copyInstall = async () => {
    try {
      await navigator.clipboard.writeText(installCommand);
      setInstallCopy("copied");
    } catch {
      setInstallCopy("failed");
    }
  };
  const machineStatus = !host
    ? "Not connected"
    : !isPinned(host)
      ? "Identity needs repair"
      : offline
        ? "Offline"
        : online.isPending
          ? "Checking connection…"
          : online.isError
            ? "Connection check failed"
            : "Online";
  const browserStatus = ready
    ? "Approved for reading and starting sessions"
    : reviewNeeded
      ? "Needs review — permissions are limited"
      : hostBusy
        ? "Machine busy — checking again shortly"
        : refusalReview
          ? insufficientPermissions
            ? "Needs review — current permissions are insufficient"
            : "Approval or permissions need review"
          : !host || !connected
            ? "Waiting for machine connection"
            : checking
              ? "Checking browser approval…"
              : mode === "passkey" && approvalNeeded
                ? "Locked — unlock with your passkey"
                : waiting
                  ? "Waiting for approval on your machine"
                  : problem
                    ? "Could not verify approval"
                    : "Not approved";

  return (
    <section aria-labelledby="host-setup-heading" className="space-y-5">
      <div>
        <h2 id="host-setup-heading" className="text-lg font-semibold">
          Set up your machine
        </h2>
        <p className="mt-1 text-sm text-muted-foreground">
          Connect a machine, then approve this browser on that machine. Being
          online does not grant browser access.
        </p>
      </div>
      {pairing.hosts.length > 1 ? (
        <label className="block text-sm">
          Machine
          <select
            className="ml-2 rounded border bg-background p-1"
            value={host?.id ?? ""}
            onChange={(event) => selectHost(event.target.value)}
          >
            {pairing.hosts.map((candidate) => (
              <option key={candidate.id} value={candidate.id}>
                {candidate.label}
              </option>
            ))}
          </select>
        </label>
      ) : (
        host && <p className="text-sm font-medium">Machine: {host.label}</p>
      )}
      <ol
        aria-label="Host setup progress"
        className="grid gap-3 sm:grid-cols-3"
      >
        <Step
          title="Machine connected"
          status={machineStatus}
          complete={connected}
          current={!connected}
        />
        <Step
          title="Browser approved"
          status={browserStatus}
          complete={verified}
          current={connected && !verified}
        />
        <Step
          title="Ready to use"
          status={
            ready ? "Read history and start sessions" : "Finish the steps above"
          }
          complete={ready}
          current={verified && !ready}
        />
      </ol>
      {!host && (
        <ol
          aria-label="Connect a machine"
          className="list-decimal space-y-4 pl-5 text-sm"
        >
          <li aria-labelledby="host-cli-download-heading" className="space-y-2">
            <p id="host-cli-download-heading" className="font-medium">
              Install the conversations CLI on that machine
            </p>
            <p className="text-muted-foreground">
              Skip this if it is already installed. The Bash installer ends by
              printing the exact login command for this server.
            </p>
            <div className="flex flex-wrap gap-2">
              <a
                className={buttonVariants({ variant: "outline", size: "sm" })}
                href="https://github.com/SjuulArtreides/conversations/releases/latest"
                target="_blank"
                rel="noopener noreferrer"
              >
                Download Conversations
              </a>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void copyInstall()}
              >
                Install with Bash
              </Button>
            </div>
            {installCopy === "copied" && (
              <p role="status" className="text-xs">
                Install command copied to the clipboard.
              </p>
            )}
            {installCopy === "failed" && (
              <div role="status" className="space-y-1 text-xs">
                <p>Could not copy. Select and copy the command below.</p>
                <code className="block break-all select-all font-mono">
                  {installCommand}
                </code>
              </div>
            )}
            <p className="text-xs text-muted-foreground">
              Or{" "}
              <a
                className="underline"
                href="https://github.com/SjuulArtreides/conversations#run-from-source"
                target="_blank"
                rel="noopener noreferrer"
              >
                build from source
              </a>
              .
            </p>
          </li>
          <li className="space-y-1">
            <p className="font-medium">
              Log in to this server and start the host
            </p>
            {loginCommand ? (
              <>
                <p className="text-muted-foreground">
                  On that machine, log in to this server. Login offers to start
                  the host; keep it running.
                </p>
                <code
                  aria-label="Login command"
                  className="block break-all select-all rounded bg-muted p-2 font-mono text-xs"
                >
                  {loginCommand}
                </code>
                <p className="text-muted-foreground">
                  Already logged in to this server, or it does not require
                  login? Start the host directly:
                </p>
                <code
                  aria-label="Host command"
                  className="block break-all select-all rounded bg-muted p-2 font-mono text-xs"
                >
                  {hostCommand}
                </code>
              </>
            ) : (
              <p className="text-muted-foreground">
                On that machine, run{" "}
                <code>conversations login --server &lt;URL&gt;</code> with this
                server&apos;s web address, then keep the host running. Without{" "}
                <code>--server</code> the CLI uses http://localhost:8080.
              </p>
            )}
            <p className="text-xs text-muted-foreground">
              If <code>conversations</code> is not on your PATH, use the full
              path the installer printed.
            </p>
          </li>
          <li className="space-y-1">
            <p className="font-medium">Copy the public host descriptor</p>
            <p className="text-muted-foreground">
              In another terminal on that machine, run the descriptor command
              that <code>conversations host</code> printed at startup:
              <code className="mt-1 block break-all">
                conversations credential --runtime-dir … descriptor
              </code>
              It prints a public descriptor starting with <code>chd1.</code>.
            </p>
          </li>
          <li>
            <form
              onSubmit={(event) => void connect(event)}
              className="space-y-3"
            >
              <p className="font-medium">Paste it here and connect</p>
              <label className="block text-sm">
                Host descriptor
                <Input
                  value={descriptor}
                  onChange={(event) => setDescriptor(event.target.value)}
                  placeholder="chd1.…"
                  className="font-mono text-xs"
                />
              </label>
              <label className="block text-sm">
                Machine name
                <Input
                  value={machineName}
                  onChange={(event) => setMachineName(event.target.value)}
                  placeholder="My laptop or Coder"
                />
              </label>
              <Button type="submit" disabled={!descriptor.trim()}>
                Connect machine
              </Button>
              {pairingError && (
                <p role="alert" className="text-sm text-destructive">
                  {pairingError}
                </p>
              )}
            </form>
          </li>
          <li className="space-y-1">
            <p className="font-medium">Prepare and approve this browser</p>
            <p className="text-muted-foreground">
              After connecting, this page asks you to prepare this browser, then
              shows how to approve it on that machine.
            </p>
          </li>
        </ol>
      )}
      {host && !isPinned(host) && (
        <p role="alert" className="text-sm text-destructive">
          This machine has no pinned identity key. Open Advanced, forget it, and
          connect it again with its public descriptor.
        </p>
      )}
      {offline && (
        <p role="status" className="text-sm">
          Next: start <code>conversations host</code> on{" "}
          <strong>{host!.label}</strong> and keep it running. This page checks
          automatically while open.
        </p>
      )}
      {checking && (
        <p role="status" className="text-sm">
          {online.isPending
            ? "Checking the machine connection…"
            : "Checking this browser’s access with the machine…"}
        </p>
      )}
      {problem && (
        <p role="alert" className="text-sm text-destructive">
          Could not verify this connection: {(problem as Error).message}
        </p>
      )}
      {(!host || mode === "browser-key") && !ready && (
        <div className="space-y-3">
          <label className="block text-sm">
            Browser name
            <Input
              value={browserName}
              maxLength={80}
              onChange={(event) => {
                setBrowserName(event.target.value);
                setNameUpdated(false);
              }}
            />
          </label>
          {approvalNeeded && !waiting && (
            <>
              <p className="text-sm">
                <strong>Next: prepare this browser for approval.</strong> This
                shares only this browser’s public key so the owner of{" "}
                <strong>{host!.label}</strong> can compare and approve it on
                that machine. It does not grant access by itself.
              </p>
              <Button
                disabled={
                  publication.key.isPending ||
                  publication.key.isError ||
                  publication.registration.isPending ||
                  publication.publish.isPending ||
                  !browserName.trim()
                }
                onClick={() => publishBrowser()}
              >
                {publication.publish.isPending
                  ? "Preparing browser…"
                  : "Prepare this browser for approval"}
              </Button>
            </>
          )}
          {awaitingApproval && !waiting && approvalNeeded && (
            <Button
              variant="outline"
              size="sm"
              disabled={rechecking}
              onClick={() => void checkApproval()}
            >
              Check browser approval
            </Button>
          )}
          {publication.registration.data === true &&
            browserName !== publishedName && (
              <Button
                size="sm"
                variant="outline"
                disabled={publication.publish.isPending || !browserName.trim()}
                onClick={() => publishBrowser(true)}
              >
                {publication.publish.isPending
                  ? "Updating browser name…"
                  : "Update browser name"}
              </Button>
            )}
          {publication.registration.data === true &&
            nameUpdated &&
            browserName === publishedName && (
              <p role="status" className="text-sm">
                Browser name updated.
              </p>
            )}
          {publication.key.error && (
            <p role="alert" className="text-sm text-destructive">
              Could not read this browser’s key: {publication.key.error.message}
            </p>
          )}
          {publication.registration.error && (
            <p role="alert" className="text-sm text-destructive">
              Could not check browser publication:{" "}
              {publication.registration.error.message}
            </p>
          )}
          {publication.publish.error && (
            <p role="alert" className="text-sm text-destructive">
              Could not prepare this browser:{" "}
              {publication.publish.error.message}
            </p>
          )}
          {waiting && publication.key.data && (
            <div className="space-y-2 rounded-lg border border-border p-4 text-sm">
              <p role="status">
                Waiting for browser approval on <strong>{host!.label}</strong>.
              </p>
              <p className="font-medium">
                Next: approve this browser on {host!.label}.
              </p>
              <p>Publication does not notify this machine or grant access.</p>
              <p>First compare this browser’s fingerprint:</p>
              <code className="block break-all select-all text-xs">
                {publication.key.data.kid}
              </code>
              <Button
                size="sm"
                variant="outline"
                onClick={() => void copy(publication.key.data!.kid)}
              >
                Copy fingerprint
              </Button>
              <section
                aria-labelledby="terminal-approval-heading"
                className="space-y-2"
              >
                <h3 id="terminal-approval-heading" className="font-medium">
                  Terminal approval instructions
                </h3>
                <p>
                  Use this for a remote machine such as Coder. In a terminal
                  on {host!.label}, run this command to approve this browser
                  with the standard owner permissions.
                </p>
                <code className="block break-all select-all text-xs">
                  {approvalCommand}
                </code>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void copy(approvalCommand)}
                >
                  Copy approval command
                </Button>
              </section>
              <section
                aria-labelledby="local-approval-heading"
                className="space-y-2"
              >
                <h3 id="local-approval-heading" className="font-medium">
                  Or approve on the local host page
                </h3>
                <p>
                  Only from a browser running on {host!.label} itself: its local
                  host page is served on that machine’s <code>localhost</code>,
                  so it does not open from another computer.
                </p>
                <ol className="list-decimal space-y-2 pl-5">
                  <li>
                    Under <strong>Access to this host</strong>, choose{" "}
                    <strong>Add person</strong> and find this browser.
                  </li>
                  <li>
                    Compare the fingerprint. Allow reading history, starting
                    sessions and controlling running terminals, then choose{" "}
                    <strong>Trust public key</strong>.
                  </li>
                </ol>
              </section>
              {copyStatus && <p role="status">{copyStatus}</p>}
              {awaitingApproval ? (
                <div className="space-y-2">
                  <p>
                    After approving this browser on {host!.label}, check
                    again here.
                  </p>
                  <Button
                    size="sm"
                    disabled={rechecking}
                    onClick={() => void checkApproval()}
                  >
                    {rechecking
                      ? "Checking browser approval…"
                      : "Check browser approval"}
                  </Button>
                </div>
              ) : (
                <p>This page detects approval automatically while open.</p>
              )}
            </div>
          )}
        </div>
      )}
      {approvalNeeded && mode === "passkey" && (
        <div className="space-y-2">
          <p className="text-sm">
            Unlock this browser’s existing passkey access to {host!.label}.
          </p>
          <Button disabled={unlocking} onClick={() => void unlock()}>
            {unlocking ? "Unlocking…" : "Unlock with passkey"}
          </Button>
          {unlockError && (
            <p role="alert" className="text-sm text-destructive">
              {unlockError}
            </p>
          )}
        </div>
      )}
      {hostBusy && (
        <p role="status" className="text-sm">
          {host!.label} is busy handling private connections. This page will
          check again automatically.
        </p>
      )}
      {awaitingApproval &&
        access.isError &&
        !identityDenied &&
        !hostBusy &&
        !refusalReview && (
          <p role="status" className="text-sm">
            The approval check could not finish. {access.error.message}
          </p>
        )}
      {refusalReview && (
        <div role="alert" className="space-y-2 text-sm text-destructive">
          <p>
            {insufficientPermissions
              ? "Permissions need review"
              : "Approval or permissions need review"}{" "}
            on {host!.label}.
          </p>
          <p>{access.error!.message}</p>
          <p>
            On {host!.label}, open <strong>Access to this host</strong> and
            review this browser’s current permissions. This check could not
            verify enough access to continue.
          </p>
        </div>
      )}
      {reviewNeeded && (
        <p role="status" className="text-sm">
          This browser’s approved permissions need review on {host!.label}.{" "}
          {access.data.canRead
            ? "History reading is allowed."
            : "History reading is not allowed."}{" "}
          {access.data.canStart
            ? "Session starts are allowed."
            : "Session starts are not allowed."}{" "}
          Ask the machine owner to review its local grants in Access to this
          host.
        </p>
      )}
      {ready && (
        <div className="space-y-3">
          <p role="status" className="text-sm">
            {host!.label} verified this browser can read history and start
            sessions.
          </p>
          <Button onClick={() => navigate("/")}>Go to sessions</Button>
        </div>
      )}
    </section>
  );
}
