import {
  isLocalHost,
  LOCAL_HOST_ID,
  localHostState,
  localRequest,
} from "./localHost";
import { localHostChannel } from "./localHostChannel";
import {
  forgetDelegation,
  fromBase64Url,
  getDelegation,
  HOST_SCOPE,
  loadBrowserKey,
  STREAM_LIFETIME_SECS,
  type OperationName,
  randomHex,
  saveDelegation,
  toBase64,
  type Delegation,
} from "./hostAuth";
import {
  activeHost,
  isPinned,
  pairedHost,
  pinIsIntact,
  type VerifiablePairedHost,
} from "./hostPairing";
import {
  checkDone,
  checkEnrolled,
  checkOffer,
  expectedPasskeyId,
  registrationDigest,
  StatementError,
  type DelegationRequest,
  type HostLaunchSettings,
  type PinnedHost,
} from "./hostStatements";
import {
  HostNetworkError,
  hostRtcConfig,
  subscribeHostNetwork,
  type HostRtcConfig,
} from "./hostNetwork";
import { createPasskey, getPasskeyAssertion } from "./passkey";
import {
  openPrivateChannel,
  PrivateChannelError,
  readSettings,
  type PrivateChannel,
  type TerminalHandlers,
  type TerminalSubscription,
  type TransferOptions,
  type TransferStatus,
} from "./privateChannel";
import type { AgentKind, AgentLauncher } from "./types";

/**
 * Signed host control: the only way the browser starts or reopens a session.
 *
 * Account login lets this page talk to the relay; it never authorizes a
 * command. The host checks a signature from a key it approved (through a
 * passkey) and tells us the outcome. Failures are reported as they are: there
 * are no retries and no fallback to an unsigned path.
 */

/** Operations the host reports as done. */
const DONE = new Set(["executed", "replayed"]);
/** Reasons that mean the stored delegation is no longer usable. */
const STALE_DELEGATION =
  /credential or delegation|delegation (revoked|expired)/i;

export class HostControlError extends Error {
  constructor(
    message: string,
    readonly code:
      | "not_paired"
      | "locked"
      | "offline"
      | "refused"
      | "failed"
      | "transport"
      | "untrusted"
      | "cancelled"
      | "uncertain",
  ) {
    super(message);
  }
}

type Reply =
  | { type: "started"; challenge: string; options: unknown; statement: unknown }
  | { type: "registered"; statement: unknown }
  | { type: "delegated"; statement: unknown }
  | { type: "error"; message: string };

type Request =
  | {
      type: "enroll_finish";
      challenge: string;
      response: unknown;
      nonce: string;
      vouch?: {
        statement: { p: string; s: string };
        host_key: string;
      };
    }
  | {
      type: "delegate_start";
      browser_pub: string;
      dlg: string;
      caps: string[];
      nonce: string;
    }
  | { type: "delegate_finish"; challenge: string; response: unknown };

/** The relay gives up after 25s; allow a little more before declaring failure. */
const REQUEST_TIMEOUT_MS = 30_000;

async function relay(hostId: string, request: Request): Promise<Reply> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  let response: Response;
  try {
    response = await fetch(
      `/api/host-relay/${encodeURIComponent(hostId)}/request`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request),
        signal: controller.signal,
      },
    );
  } catch {
    throw new HostControlError(
      "Could not reach the server. The request was not retried and may or may not have run.",
      "transport",
    );
  } finally {
    clearTimeout(timer);
  }
  const body = (await response.json().catch(() => null)) as
    (Reply & { error?: string; code?: string }) | null;
  if (!response.ok) {
    throw new HostControlError(
      body?.error ?? `The server refused the request (${response.status}).`,
      body?.code === "host_offline" ? "offline" : "transport",
    );
  }
  if (!body || typeof body.type !== "string") {
    throw new HostControlError(
      "The host sent an unreadable answer.",
      "transport",
    );
  }
  return body;
}

export async function hostOnline(hostId: string): Promise<boolean> {
  if (isLocalHost() && hostId === LOCAL_HOST_ID) {
    if (!localHostState().authenticated) return false;
    await localRequest("/api/status");
    return true;
  }
  const response = await fetch(
    `/api/host-relay/${encodeURIComponent(hostId)}/status`,
  );
  if (!response.ok) return false;
  return ((await response.json()) as { online?: boolean }).online === true;
}

function failed(reply: Reply, what: string): never {
  throw new HostControlError(
    reply.type === "error"
      ? reply.message
      : `Unexpected host answer to ${what}.`,
    "refused",
  );
}

/** What a delegation may do. The host also caps this by the passkey's grants. */
export const DELEGATION_CAPS = [
  "session.start",
  "session.reopen",
  // PR refresh (pr.sync / pr.unlink) follows session.write; a delegation
  // that holds write on a session may refresh its linked pull requests
  // without a standalone pr_sync cap.
  "session.write",
  "session.manage_sharing",
  // Only an owner key the host approved for them keeps these after narrowing.
  "session.pr_comment",
  "host.settings_read",
  "host.settings_write",
  // The key-trust ceremony itself (owner-only, never grantable): invite a
  // key, see the pending offers, approve one.
  "passkey.manage",
  // Private history, search and the session list; only ever on the private channel.
  "history.read",
];

/**
 * Ask the host to accept this browser's key for 24 hours. Needs a passkey
 * approval (a user gesture) but no local approval on the host.
 */
const unlocking = new Map<string, Promise<Delegation>>();

/**
 * One unlock per host at a time: a second caller (another click, or keystrokes
 * queued behind the passkey prompt) shares the ceremony in flight instead of
 * opening a second prompt.
 */
export function unlockHost(hostId: string): Promise<Delegation> {
  const active = unlocking.get(hostId);
  if (active) return active;
  const run = performUnlock(hostId).finally(() => unlocking.delete(hostId));
  unlocking.set(hostId, run);
  return run;
}

async function performUnlock(hostId: string): Promise<Delegation> {
  const host = await pinnedHostOrThrow(hostId);
  const key = await loadBrowserKey();
  const request: DelegationRequest = {
    browserPub: toBase64(key.publicRaw),
    kid: key.kid,
    dlg: randomHex(16),
    caps: [...DELEGATION_CAPS].sort(),
    nonce: randomHex(16),
  };
  const started = await relay(hostId, {
    type: "delegate_start",
    browser_pub: request.browserPub,
    dlg: request.dlg,
    caps: request.caps,
    nonce: request.nonce,
  });
  if (started.type !== "started") failed(started, "the unlock request");
  // Reject anything that is not exactly what we asked for BEFORE the passkey
  // prompt: approving it would otherwise authorize whatever the relay chose.
  const offer = await authentic(() => checkOffer(host, request, started));
  let response;
  try {
    response = await getPasskeyAssertion(started.options);
  } catch (error) {
    throw new HostControlError(
      (error as Error)?.name === "NotAllowedError"
        ? "The passkey prompt was cancelled."
        : ((error as Error)?.message ?? "The passkey was not used."),
      "cancelled",
    );
  }
  const finished = await relay(hostId, {
    type: "delegate_finish",
    challenge: started.challenge,
    response,
  });
  if (finished.type !== "delegated") failed(finished, "the unlock");
  const done = await authentic(() =>
    checkDone(host, request, started.challenge, offer, finished.statement),
  );
  let parentKid: string;
  try {
    if (typeof response.rawId !== "string" || !response.rawId)
      throw new Error("missing passkey credential id");
    parentKid = await expectedPasskeyId(fromBase64Url(response.rawId));
  } catch {
    throw new HostControlError(
      "The passkey response did not identify its approving credential. Unlock again.",
      "refused",
    );
  }
  const delegation = {
    hostId,
    parentKid,
    dlg: request.dlg,
    kid: key.kid,
    notAfter: done.notAfter,
    caps: done.caps,
  };
  await saveDelegation(delegation);
  return delegation;
}

async function authentic<T>(check: () => Promise<T>): Promise<T> {
  try {
    return await check();
  } catch (error) {
    if (error instanceof StatementError) {
      throw new HostControlError(error.message, "untrusted");
    }
    throw error;
  }
}

/** Shown when no machine has approved this browser yet: the fix is on the machine. */
export const NOT_APPROVED_MESSAGE =
  "No machine has approved this browser yet. On your machine, open local Conversations → Settings → Host access and approve this browser's fingerprint under Waiting for approval. This browser then connects by itself.";

async function pinnedHostOrThrow(
  hostId: string,
): Promise<VerifiablePairedHost> {
  const host = pairedHost(hostId);
  if (!host) {
    throw new HostControlError(
      NOT_APPROVED_MESSAGE,
      "not_paired",
    );
  }
  if (!isPinned(host) || !(await pinIsIntact(host))) {
    throw new HostControlError(
      "This host was paired before its identity key was pinned (or its pin is damaged). Forget it and pair again with the descriptor from `credential descriptor`.",
      "not_paired",
    );
  }
  return host;
}

/** Complete a registration the host started locally (link from `passkey enroll-start`). */
/**
 * Create the passkey and have the host finish the registration. The returned
 * id is derived HERE from the authenticator's own output, and is returned only
 * if the host (under the key from the pasted descriptor) signed a completion
 * bound to this ceremony, our nonce and a digest of exactly that response.
 * Nothing the relay says is ever returned or shown.
 */
export async function finishEnrollment(
  host: PinnedHost,
  challenge: string,
  options: unknown,
  vouchFor?: (
    credId: string,
  ) => Promise<
    { statement: { p: string; s: string }; host_key: string } | undefined
  >,
): Promise<string> {
  let response;
  try {
    response = await createPasskey(options);
  } catch (error) {
    throw new HostControlError(
      (error as Error)?.name === "NotAllowedError"
        ? "The passkey prompt was cancelled."
        : ((error as Error)?.message ?? "No passkey was created."),
      "cancelled",
    );
  }
  const sent = response as {
    rawId: string;
    response: { attestationObject: string; clientDataJSON: string };
  };
  let rawId, attestationObject, clientDataJSON;
  try {
    rawId = fromBase64Url(sent.rawId);
    attestationObject = fromBase64Url(sent.response.attestationObject);
    clientDataJSON = fromBase64Url(sent.response.clientDataJSON);
  } catch {
    throw new HostControlError(
      "The passkey response was unreadable.",
      "refused",
    );
  }
  const expected = {
    ceremony: challenge,
    nonce: randomHex(16),
    credId: await expectedPasskeyId(rawId),
    responseDigest: await registrationDigest({
      rawId,
      attestationObject,
      clientDataJSON,
    }),
  };
  // The vouch is made HERE: after the authenticator produced the key (so the
  // passkey id the host will derive is known) and before anything is
  // submitted — the offering host's signature binds exactly this key, and a
  // vouch for any other key is refused by the enrolling host. A failed vouch
  // aborts the offer: no unvouched enrollment slips through when the
  // colleague's host was expected to speak.
  let vouch:
    { statement: { p: string; s: string }; host_key: string } | undefined;
  if (vouchFor) {
    try {
      vouch = await vouchFor(expected.credId);
    } catch (error) {
      throw new HostControlError(
        `No key was offered: your own host could not vouch for it — ${(error as Error).message}`,
        "refused",
      );
    }
  }
  const reply = await relay(host.id, {
    type: "enroll_finish",
    challenge,
    response,
    nonce: expected.nonce,
    vouch,
  });
  if (reply.type !== "registered") failed(reply, "the registration");
  return authentic(() => checkEnrolled(host, expected, reply.statement));
}

/**
 * Sign one operation and send it on the private channel only. For every
 * private control: anything that carries content to the host (a prompt,
 * keystrokes, settings, a start request, a pull request target or comment,
 * file names and hashes) and the session's lifecycle, grants and publication.
 * Nothing here falls back to the relay.
 */
async function channelOperation(
  hostId: string,
  op: OperationName,
  session: string,
  args?: Record<string, unknown>,
  lifetime?: number,
  allowUnlock = true,
): Promise<string | null> {
  const result = await withPrivateChannel(
    hostId,
    (channel) => channel.operation(op, session, args, { lifetime }),
    allowUnlock,
  );
  return outcomeOf(hostId, result.status, result.detail);
}

async function outcomeOf(
  hostId: string,
  status: string,
  detailOrNull: string | null,
): Promise<string | null> {
  if (DONE.has(status)) return detailOrNull;
  const detail = detailOrNull ?? "";
  if (status === "refused") {
    if (STALE_DELEGATION.test(detail)) await forgetDelegation(hostId);
    throw new HostControlError(
      `The host refused this: ${detail || "not authorized"}.`,
      "refused",
    );
  }
  throw new HostControlError(
    status === "uncertain"
      ? "An earlier attempt was interrupted; the host will not repeat it. Check the host."
      : `The host could not complete this${detail ? `: ${detail}` : "."}`,
    "failed",
  );
}

/**
 * Replay and ordering domains. Replay is stopped by the host consuming every
 * frame's unique id durably; sequence numbers only keep ONE stream in order.
 * The signing key is shared by every tab and component, so a sequence must
 * never be global: each input channel has its own random id and counter, and
 * resize requests have their own (per tab). Input can therefore never be made
 * "stale" by another tab, a resize or a repaint.
 */
let resizeChannel: string | null = null;
let resizeSeq = 0;
function nextResize(): { ch: string; seq: number } {
  resizeChannel ??= randomHex(16);
  resizeSeq += 1;
  return { ch: resizeChannel, seq: resizeSeq };
}

const MAX_INPUT_CHARS = 1000;
const MAX_REPLY_BYTES = 4096;

/** `text` cut to at most `limit` UTF-8 bytes, on a character boundary. */
export function clampUtf8(text: string, limit: number): string {
  const encoder = new TextEncoder();
  if (encoder.encode(text).length <= limit) return text;
  let out = "";
  let used = 0;
  for (const char of text) {
    const size = encoder.encode(char).length;
    if (used + size > limit) break;
    out += char;
    used += size;
  }
  return out;
}

/** Split off at most `MAX_INPUT_CHARS` UTF-16 units without cutting a surrogate pair. */
function takeChunk(text: string): [string, string] {
  let end = Math.min(text.length, MAX_INPUT_CHARS);
  const last = text.charCodeAt(end - 1);
  if (end < text.length && last >= 0xd800 && last <= 0xdbff) end -= 1;
  return [text.slice(0, end), text.slice(end)];
}

/** This page's channel to each host, opened under that host's network
 * settings at `revision`. */
interface ChannelEntry {
  authMode: "passkey" | "browser-key";
  revision: string;
  opening: Promise<PrivateChannel>;
}
const channels = new Map<string, ChannelEntry>();

/** Close one host's channel; a channel still opening is closed when it opens. */
function dropChannel(hostId: string) {
  const entry = channels.get(hostId);
  channels.delete(hostId);
  void entry?.opening.then((c) => c.close()).catch(() => undefined);
}

let watchingNetwork = false;

/** Saving or clearing a host's network settings closes its channel, so the
 * next operation connects under the new settings and never the old ones. */
function watchNetwork() {
  if (watchingNetwork) return;
  watchingNetwork = true;
  subscribeHostNetwork(() => {
    for (const [hostId, entry] of [...channels]) {
      let revision: string | null = null;
      try {
        revision = hostRtcConfig(hostId).revision;
      } catch {
        // Corrupt settings: close, and the next open fails closed.
      }
      if (revision !== entry.revision) dropChannel(hostId);
    }
  });
}

/**
 * Run `read` on this page's private channel to `hostId`, opening one (and
 * without prompting by default) when none is open. User actions may opt in to unlock. Errors keep their
 * meaning: a channel that cannot be opened is `transport`; nothing is retried
 * over the relay.
 */
export async function withPrivateChannel<T>(
  hostId: string,
  read: (channel: PrivateChannel) => Promise<T>,
  allowUnlock = false,
): Promise<T> {
  if (isLocalHost() && hostId === LOCAL_HOST_ID) return read(localHostChannel);
  let entry: ChannelEntry | undefined;
  try {
    const [channel, opened] = await privateChannel(hostId, allowUnlock);
    entry = opened;
    return await read(channel);
  } catch (error) {
    if (error instanceof PrivateChannelError) {
      if (
        error.code === "unavailable" &&
        entry &&
        channels.get(hostId) === entry
      )
        channels.delete(hostId);
      throw new HostControlError(
        error.message,
        error.code === "unavailable"
          ? "transport"
          : error.code === "untrusted"
            ? "untrusted"
            : "refused",
      );
    }
    if (error instanceof StatementError) {
      throw new HostControlError(error.message, "untrusted");
    }
    if (error instanceof HostNetworkError) {
      throw new HostControlError(
        "This browser's network settings for the host cannot be used. Fix or clear them in Settings.",
        "transport",
      );
    }
    throw error;
  }
}

/** The open channel to `hostId`, and the cache entry it came from. */
async function privateChannel(
  hostId: string,
  allowUnlock: boolean,
): Promise<[PrivateChannel, ChannelEntry]> {
  watchNetwork();
  const config = hostRtcConfig(hostId);
  const authMode = (await pinnedHostOrThrow(hostId)).authMode ?? "passkey";
  const cached = channels.get(hostId);
  if (
    cached &&
    cached.revision === config.revision &&
    cached.authMode === authMode
  ) {
    const channel = await cached.opening.catch(() => null);
    if (hostRtcConfig(hostId).revision !== config.revision)
      throw settingsChanged();
    const current = channels.get(hostId);
    if (current && current !== cached)
      return privateChannel(hostId, allowUnlock);
    if (channel?.open) return [channel, cached];
    channels.delete(hostId);
  } else if (cached) {
    dropChannel(hostId);
  }
  const entry: ChannelEntry = {
    authMode,
    revision: config.revision,
    opening: openChannel(hostId, config, allowUnlock),
  };
  channels.set(hostId, entry);
  entry.opening.catch(() => {
    if (channels.get(hostId) === entry) channels.delete(hostId);
  });
  const channel = await entry.opening;
  if (channels.get(hostId) !== entry) {
    channel.close();
    throw settingsChanged();
  }
  return [channel, entry];
}

const settingsChanged = () =>
  new HostControlError(
    "The network settings for this host changed while connecting. Try again.",
    "transport",
  );

async function openChannel(
  hostId: string,
  config: HostRtcConfig,
  allowUnlock: boolean,
): Promise<PrivateChannel> {
  const host = await pinnedHostOrThrow(hostId);
  let delegation =
    host.authMode === "browser-key" ? null : await getDelegation(hostId);
  if (host.authMode !== "browser-key" && !delegation) {
    if (!allowUnlock)
      throw new HostControlError(
        "Unlock this device with your passkey to read private sessions.",
        "locked",
      );
    delegation = await unlockHost(hostId);
  }
  const key = await loadBrowserKey();
  if (hostRtcConfig(hostId).revision !== config.revision)
    throw settingsChanged();
  return openPrivateChannel({
    host,
    key,
    dlg: delegation?.dlg ?? null,
    iceServers: config.iceServers,
    relayOnly: config.iceTransportPolicy === "relay",
  });
}

/**
 * Unlock `hostId` again after it refused or withdrew this browser's
 * authority: a fresh passkey approval (call it from a user gesture), then a
 * new private channel under the new delegation. A host that removed this
 * browser refuses the unlock, so nothing is resurrected.
 */
export async function unlockAgain(hostId: string): Promise<void> {
  dropChannel(hostId);
  await forgetDelegation(hostId);
  const host = await pinnedHostOrThrow(hostId);
  if (host.authMode !== "browser-key") await unlockHost(hostId);
}

/** Close this page's private channels (on sign-out or unpairing). */
export function closePrivateChannels(): void {
  if (isLocalHost()) localHostChannel.close();
  for (const hostId of [...channels.keys()]) dropChannel(hostId);
}

function pairedHostOrThrow(): string {
  if (isLocalHost()) return LOCAL_HOST_ID;
  const host = activeHost();
  if (!host) {
    throw new HostControlError(
      NOT_APPROVED_MESSAGE,
      "not_paired",
    );
  }
  return host.id;
}

/** `hostId` itself, if this browser paired it; never another host. */
function pairedOrThrow(hostId: string): string {
  if (isLocalHost() && hostId === LOCAL_HOST_ID) return hostId;
  if (!pairedHost(hostId)) {
    throw new HostControlError(
      "That host is not paired with this browser.",
      "not_paired",
    );
  }
  return hostId;
}

function hostOrActive(hostId?: string): string {
  return hostId === undefined ? pairedHostOrThrow() : pairedOrThrow(hostId);
}

/** Who a published session is shared with, exactly as the host expects it.
 * `write_pending` names readers also intended to type into the session
 * through the host once their key is trusted there (EX-02): until then they
 * read the published copy like everyone else and cannot write. */
export type PublicationAudience =
  | { visibility: "organization" }
  | { visibility: "restricted"; emails: string[]; write_pending?: string[] };

/** The host's own account of a session's publication. */
export type PublicationState =
  "private" | "syncing" | "shared" | "deletion_pending" | "error";

const PUBLICATION_STATES: readonly string[] = [
  "private",
  "syncing",
  "shared",
  "deletion_pending",
  "error",
];

/** The host refuses anything but its canonical form: trimmed, lower-case, sorted, unique. */
function canonicalAudience(audience: PublicationAudience): PublicationAudience {
  if (audience.visibility === "organization")
    return { visibility: "organization" };
  const emails = [
    ...new Set(
      audience.emails.map((e) => e.trim().toLowerCase()).filter(Boolean),
    ),
  ].sort();
  // A pending writer must be a reader; the host enforces the same rule.
  const write_pending = [
    ...new Set(
      (audience.write_pending ?? [])
        .map((e) => e.trim().toLowerCase())
        .filter(Boolean)
        .filter((e) => emails.includes(e)),
    ),
  ].sort();
  return write_pending.length
    ? { visibility: "restricted", emails, write_pending }
    : { visibility: "restricted", emails };
}

/** One verified registration waiting for local approval on a host — a key
 * offer. The host records the trust only when the owner approves it.
 * `vouch_host_*` names the host that vouched for the enrollment (EX-04);
 * `trusted_host` marks offers whose vouching host already has an approved
 * key here: the pair was verified before (EX-05 inherited trust). */
export interface PasskeyOffer {
  cred_id: string;
  label: string;
  kind: string;
  grants: string[];
  vouch_host_id?: string;
  vouch_host_key?: string;
  trusted_host?: boolean;
}

/** The host's signed account of a session's publication, from `publication.status`. */
export interface PublicationStatus {
  state: PublicationState;
  rev: number;
  /** Who may read the published copy; `null` when nothing is published. */
  audience: PublicationAudience | null;
  /** Messages the server holds at their current local version. */
  published: number;
  /** Local messages not yet acknowledged by the server. */
  pending: number;
  /** Why publishing stopped, when the host says. */
  error: string | null;
}

function untrustedStatus(): never {
  throw new HostControlError(
    "The host answered with an unknown sharing state.",
    "untrusted",
  );
}

function publicationState(detail: string | null): PublicationState {
  if (detail && PUBLICATION_STATES.includes(detail)) {
    return detail as PublicationState;
  }
  untrustedStatus();
}

const isCount = (value: unknown): value is number =>
  Number.isSafeInteger(value) && (value as number) >= 0;

function audienceOf(value: unknown): PublicationAudience | null {
  if (value === null) return null;
  if (typeof value !== "object" || Array.isArray(value)) untrustedStatus();
  const { visibility, emails, write_pending, ...rest } = value as Record<
    string,
    unknown
  >;
  if (Object.keys(rest).length) untrustedStatus();
  if (visibility === "organization" && emails === undefined)
    return { visibility: "organization" };
  if (
    visibility === "restricted" &&
    Array.isArray(emails) &&
    emails.every((e) => typeof e === "string" && e.length > 0) &&
    (write_pending === undefined ||
      (Array.isArray(write_pending) &&
        write_pending.every((e) => typeof e === "string" && e.length > 0)))
  ) {
    return write_pending === undefined
      ? { visibility: "restricted", emails: [...emails] }
      : {
          visibility: "restricted",
          emails: [...emails],
          write_pending: [...(write_pending as string[])],
        };
  }
  untrustedStatus();
}

/** Accept the host's status only in exactly the shape it signs; never default. */
function publicationStatusOf(detail: string | null): PublicationStatus {
  let parsed: unknown;
  try {
    parsed = JSON.parse(detail ?? "");
  } catch {
    untrustedStatus();
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed))
    untrustedStatus();
  const { state, rev, audience, published, pending, error } = parsed as Record<
    string,
    unknown
  >;
  if (
    typeof state !== "string" ||
    !isCount(rev) ||
    !isCount(published) ||
    !isCount(pending) ||
    (error !== undefined && error !== null && typeof error !== "string")
  ) {
    untrustedStatus();
  }
  return {
    state: publicationState(state),
    rev,
    audience: audienceOf(audience),
    published,
    pending,
    error: (error as string | null | undefined) ?? null,
  };
}

export type {
  TerminalEnd,
  TerminalEndReason,
  TerminalFrame,
  TerminalHandlers,
  TerminalSubscription,
  TransferOptions,
  TransferStatus,
} from "./privateChannel";

const TRANSFER_FAILURES: Record<
  Exclude<TransferStatus, "delivered">,
  [string, HostControlError["code"]]
> = {
  // The host refuses and cancels only before it hands anything to the terminal.
  refused: ["The host refused this transfer. Nothing was pasted.", "refused"],
  cancelled: ["The transfer was cancelled. Nothing was pasted.", "cancelled"],
  // A delivery can fail after some paths were already pasted.
  failed: [
    "The transfer did not finish. Some of it may have been pasted; check the terminal before trying again.",
    "failed",
  ],
  // Handed to the terminal, which never reported back: it may be pasted, so
  // it is not sent again.
  uncertain: [
    "Your host received it, but the terminal did not confirm the paste. Check the terminal before sending it again.",
    "uncertain",
  ],
};

function transferOutcome(status: TransferStatus): void {
  if (status === "delivered") return;
  const [message, code] = TRANSFER_FAILURES[status];
  throw new HostControlError(message, code);
}

/**
 * Sign one enrollment-ceremony vouch with the ACTIVE host's identity key, for
 * a ceremony on another host (EX-04): the colleague's own host speaks for the
 * key they are offering. Admitted against `host.settings_read`; it names no
 * session and grants nothing. Answers `{ statement, host_key }`.
 */
export async function passkeyVouch(
  forHost: string,
  challenge: string,
  credId: string,
  sourceHostId?: string,
): Promise<{ statement: { p: string; s: string }; host_key: string }> {
  const detail = await channelOperation(
    sourceHostId === undefined
      ? pairedHostOrThrow()
      : pairedOrThrow(sourceHostId),
    "passkey.vouch",
    HOST_SCOPE,
    { for_host: forHost, challenge, cred_id: credId },
  );
  const parsed = JSON.parse(detail ?? "{}") as {
    statement?: { p?: string; s?: string };
    host_key?: string;
  };
  if (
    typeof parsed.statement?.p !== "string" ||
    typeof parsed.statement?.s !== "string" ||
    typeof parsed.host_key !== "string"
  ) {
    throw new HostControlError("the host answered without a vouch", "refused");
  }
  return {
    statement: parsed.statement as { p: string; s: string },
    host_key: parsed.host_key,
  };
}

/**
 * Pair `peer`, another of the owner's machines, on `hostId`: the peer gets
 * full control there. `peer.key` must be the identity key this browser pinned
 * for it, never one taken from the server; the host checks that it signed
 * `peer.card` and that this browser's key itself has full control.
 */
export async function pairMachine(
  hostId: string,
  peer: { key: string; card: { p: string; s: string } },
): Promise<string> {
  return (
    (await channelOperation(pairedOrThrow(hostId), "machine.pair", HOST_SCOPE, {
      host_key: peer.key,
      card: peer.card,
    })) ?? ""
  );
}

export const hostControl = {
  async exportSessionChunk(
    hostId: string,
    session: string,
    transfer: string,
    offset: number,
  ) {
    const result = await withPrivateChannel(
      hostOrActive(hostId),
      (channel) =>
        channel.operation("migration.export", session, { transfer, offset }),
      true,
    );
    await outcomeOf(hostId, result.status, result.detail);
    if (result.body?.kind !== "migration")
      throw new HostControlError(
        "The host returned no migration snapshot.",
        "untrusted",
      );
    return result.body;
  },
  async importSessionChunk(
    hostId: string,
    input: {
      transfer: string;
      destination: string;
      cwd: string;
      manifest: import("./privateChannel").MigrationManifest;
      offset: number;
      b64: string;
    },
  ): Promise<boolean> {
    const detail = await channelOperation(
      hostOrActive(hostId),
      "migration.import",
      HOST_SCOPE,
      input,
    );
    if (detail !== "copied" && detail !== "receiving")
      throw new HostControlError(
        "The host did not confirm the migration state.",
        "uncertain",
      );
    return detail === "copied";
  },
  /** Start a new session. The browser names it; the host decides everything else. */
  async startSession(
    agent: AgentKind,
    launcher?: AgentLauncher,
  ): Promise<{ delivered: true; sessionId: string }> {
    const hostId = pairedHostOrThrow();
    const sessionId = crypto.randomUUID();
    await channelOperation(hostId, "session.start", sessionId, {
      agent,
      ...(launcher && launcher !== "opencode" ? { launcher } : {}),
    });
    return { delivered: true, sessionId };
  },

  /** Reopen a session; the host resolves its folder and resume id locally. */
  async reopenSession(
    sessionId: string,
    hostId?: string,
  ): Promise<{ delivered: true }> {
    await channelOperation(hostOrActive(hostId), "session.reopen", sessionId);
    return { delivered: true };
  },

  async renameSession(
    sessionId: string,
    title: string,
    hostId?: string,
  ): Promise<string> {
    const effective = await channelOperation(
      hostOrActive(hostId),
      "session.rename",
      sessionId,
      { session: sessionId, title },
    );
    if (effective === null)
      throw new HostControlError(
        "The host did not return the renamed title.",
        "failed",
      );
    return effective;
  },

  /**
   * Send a message to a session: sanitized text plus Enter, signed and
   * journaled. Longer text is cut to the signed-message limit (as the old
   * reply path cut to its cap), never split inside a character.
   */
  async sendReply(
    sessionId: string,
    text: string,
    hostId?: string,
  ): Promise<void> {
    await channelOperation(hostOrActive(hostId), "session.reply", sessionId, {
      text: clampUtf8(text, MAX_REPLY_BYTES),
    });
  },

  /**
   * Send ONE pasted image to the session over the private channel: the host
   * checks the signed type, size and SHA-256, stages it privately for this
   * session and pastes its path (never Enter, never the machine clipboard).
   * There is no other path for the bytes.
   */
  async pasteImage(
    sessionId: string,
    mime: string,
    bytes: Uint8Array,
    opts: TransferOptions = {},
    hostId?: string,
  ): Promise<void> {
    transferOutcome(
      await withPrivateChannel(
        hostOrActive(hostId),
        (channel) => channel.pasteImage(sessionId, mime, bytes, opts),
        true,
      ),
    );
  },

  /** Same for ONE file drop: exact names, order, sizes and SHA-256s. */
  async sendFiles(
    sessionId: string,
    files: { name: string; bytes: Uint8Array }[],
    opts: TransferOptions = {},
    hostId?: string,
  ): Promise<void> {
    transferOutcome(
      await withPrivateChannel(
        hostOrActive(hostId),
        (channel) => channel.sendFiles(sessionId, files, opts),
        true,
      ),
    );
  },

  /**
   * Follow ONE session's terminal over the private channel: a snapshot, then
   * updates, until the host ends it (session end, revoked access) or `close`.
   */
  async subscribeTerminal(
    hostId: string,
    sessionId: string,
    handlers: TerminalHandlers,
  ): Promise<TerminalSubscription> {
    return withPrivateChannel(pairedOrThrow(hostId), (channel) =>
      channel.subscribeTerminal(sessionId, handlers),
    );
  },

  /**
   * Ask the host to re-read ONE pull request of this session with its own `gh`.
   * The signed operation names the exact repo and number; the host never takes a
   * target from the server.
   */
  async syncPr(
    sessionId: string,
    target: { owner: string; repo: string; number: number },
    hostId?: string,
  ): Promise<void> {
    await channelOperation(hostOrActive(hostId), "pr.sync", sessionId, {
      owner: target.owner,
      repo: target.repo,
      number: target.number,
    });
  },

  /**
   * Post ONE comment (or a reply in a review thread) as the host's GitHub user.
   * Repo, number, thread and body are all inside the signed operation.
   */
  async commentOnPr(
    sessionId: string,
    comment: {
      owner: string;
      repo: string;
      number: number;
      body: string;
      in_reply_to?: number;
    },
    hostId?: string,
  ): Promise<void> {
    await channelOperation(hostOrActive(hostId), "pr.comment", sessionId, {
      owner: comment.owner,
      repo: comment.repo,
      number: comment.number,
      body: comment.body,
      ...(comment.in_reply_to != null
        ? { in_reply_to: comment.in_reply_to }
        : {}),
    });
  },

  /**
   * Read this host's launch flags over the private channel: a host-signed reply
   * on a channel the pinned host answered. There is no relay or central copy.
   */
  async getHostSettings(hostId?: string): Promise<HostLaunchSettings> {
    return withPrivateChannel(hostOrActive(hostId), readSettings, false);
  },

  /**
   * Run a read on the private channel to `hostId` (no relay fallback). The
   * caller names the host, so a page never reads one host's session from
   * another that happens to be active.
   */
  async privateRead<T>(
    hostId: string,
    read: (channel: PrivateChannel) => Promise<T>,
    options?: { allowUnlock?: boolean },
  ): Promise<T> {
    return withPrivateChannel(
      pairedOrThrow(hostId),
      read,
      options?.allowUnlock ?? false,
    );
  },

  /**
   * Replace this host's launch flags (the whole block; nothing else on the host
   * can be named by this operation).
   */
  async updateHostSettings(
    launch: HostLaunchSettings,
    hostId?: string,
  ): Promise<void> {
    await channelOperation(
      hostOrActive(hostId),
      "settings.update",
      HOST_SCOPE,
      {
        launch,
      },
    );
  },

  /**
   * Begin a passkey enrollment on this host for `label` (owner-only
   * `passkey.manage`) and answer with the invite link to hand to that person.
   * The link carries everything in its fragment; it grants nothing — the
   * registration waits for {@link hostControl.passkeyApprove}.
   */
  async passkeyInvite(
    hostId: string,
    label: string,
    grants: string[],
  ): Promise<string> {
    const detail = await channelOperation(
      hostId,
      "passkey.enroll_start",
      HOST_SCOPE,
      { label, kind: "agent", grants },
    );
    const parsed = JSON.parse(detail ?? "{}") as { link?: string };
    if (typeof parsed.link !== "string" || !parsed.link) {
      throw new HostControlError(
        "the host answered without an invite link",
        "refused",
      );
    }
    return parsed.link;
  },

  /**
   * The verified registrations waiting for approval on this host: the key
   * offers it is holding (label, kind, grants per offer).
   */
  async passkeyPending(hostId: string): Promise<PasskeyOffer[]> {
    const detail = await channelOperation(
      hostId,
      "passkey.pending",
      HOST_SCOPE,
    );
    const parsed = JSON.parse(detail ?? "[]") as PasskeyOffer[];
    if (!Array.isArray(parsed)) {
      throw new HostControlError(
        "the host answered without pending keys",
        "refused",
      );
    }
    return parsed;
  },

  /**
   * Approve one pending registration: the host records the trust locally
   * (UI-based local approval — the host, never the browser or the server, is
   * the authority).
   */
  async passkeyApprove(hostId: string, credId: string): Promise<void> {
    await channelOperation(hostId, "passkey.approve", HOST_SCOPE, {
      cred_id: credId,
    });
  },

  async trustPublishedBrowser(
    hostId: string,
    request: {
      kid: string;
      public_key: string;
      label: string;
      grants: string[];
      compared: boolean;
    },
  ): Promise<{ kid: string }> {
    const started = JSON.parse(
      (await channelOperation(
        hostId,
        "browser_trust.start",
        HOST_SCOPE,
        request,
      )) ?? "null",
    );
    if (typeof started?.challenge !== "string" || !started.options) {
      throw new HostControlError(
        "The host did not return an approval challenge.",
        "refused",
      );
    }
    const response = await getPasskeyAssertion(started.options);
    const approved = JSON.parse(
      (await channelOperation(
        hostId,
        "browser_trust.finish",
        HOST_SCOPE,
        {
          challenge: started.challenge,
          response,
        },
        undefined,
        false,
      )) ?? "null",
    );
    if (approved?.kid !== request.kid) {
      throw new HostControlError(
        "The host did not confirm the selected browser key.",
        "refused",
      );
    }
    return { kid: request.kid };
  },

  /**
   * Publish this session's local history to `audience`, or change who may
   * read it. Only the host that publishes it can do this (`hostId` comes from
   * the session's publication record); it records a newer revision and
   * uploads in the background, so the answer is usually "syncing".
   */
  async sharePublication(
    hostId: string,
    sessionId: string,
    audience: PublicationAudience,
  ): Promise<PublicationState> {
    return publicationState(
      await channelOperation(hostId, "publication.share", sessionId, {
        audience: canonicalAudience(audience),
      }),
    );
  },

  /** Stop publishing and ask the server to delete the published copy. */
  async unsharePublication(
    hostId: string,
    sessionId: string,
  ): Promise<PublicationState> {
    return publicationState(
      await channelOperation(hostId, "publication.unshare", sessionId),
    );
  },

  /** The host's current state, revision, audience and counts for one session. */
  async publicationStatus(
    hostId: string,
    sessionId: string,
  ): Promise<PublicationStatus> {
    return publicationStatusOf(
      await channelOperation(
        hostId,
        "publication.status",
        sessionId,
        undefined,
        undefined,
        false,
      ),
    );
  },

  /** End the session's terminal on the host. */
  async endSession(sessionId: string, hostId?: string): Promise<void> {
    await channelOperation(hostOrActive(hostId), "session.end", sessionId);
  },

  async resizeSession(
    sessionId: string,
    cols: number,
    rows: number,
    hostId?: string,
  ): Promise<void> {
    await channelOperation(
      hostOrActive(hostId),
      "session.resize",
      sessionId,
      { ...nextResize(), cols, rows },
      STREAM_LIFETIME_SECS,
    );
  },

  async repaintSession(sessionId: string, hostId?: string): Promise<void> {
    await channelOperation(
      hostOrActive(hostId),
      "session.repaint",
      sessionId,
      {},
      STREAM_LIFETIME_SECS,
    );
  },

  /**
   * Let an existing, locally approved credential write to ONE session. The host
   * decides whether this is allowed (your management authority for that
   * session, the recipient's approved ceiling); nothing here can add a
   * credential or raise a ceiling.
   */
  async addGrant(
    sessionId: string,
    recipientCredential: string,
    caps: string[] = ["session.write"],
    hostId?: string,
  ): Promise<void> {
    await channelOperation(hostOrActive(hostId), "grant.add", sessionId, {
      recipient: recipientCredential.trim(),
      caps: [...new Set(caps)].sort(),
    });
  },

  async listAccess(hostId?: string) {
    const host = hostOrActive(hostId);
    const result = await withPrivateChannel(host, (channel) =>
      channel.operation("access.list", HOST_SCOPE, {}),
    );
    await outcomeOf(host, result.status, result.detail);
    if (result.body?.kind !== "access")
      throw new HostControlError(
        "The host returned unreadable credential metadata.",
        "untrusted",
      );
    return result.body.credentials;
  },

  /**
   * Take capabilities on ONE session back from a credential. Decided by the
   * host like `addGrant`; removing write ends that key's write on the session.
   */
  async removeGrant(
    sessionId: string,
    recipientCredential: string,
    caps: string[],
    hostId?: string,
  ): Promise<void> {
    await channelOperation(hostOrActive(hostId), "grant.remove", sessionId, {
      recipient: recipientCredential.trim(),
      caps: [...new Set(caps)].sort(),
    });
  },
};

export interface InputChannel {
  /** Queue raw terminal input. Order is preserved; never retried. */
  send(text: string): void;
  close(): void;
}

/**
 * Signed keystroke input for one session. Input is coalesced while a frame is
 * in flight (one frame at a time keeps the order), and each frame is signed
 * with the browser's temporary key, so typing never prompts for a passkey
 * (only the 24-hour unlock does). The host checks the delegation, the parent's
 * current grant on EVERY frame and consumes its id durably, so revoking stops the next one.
 */
export function createInputChannel(
  sessionId: string,
  onError: (error: HostControlError) => void,
  hostId?: string,
): InputChannel {
  const ch = randomHex(16);
  let seq = 0;
  let pending = "";
  let busy = false;
  let closed = false;
  async function flush() {
    if (busy) return;
    busy = true;
    try {
      while (pending && !closed) {
        const [chunk, rest] = takeChunk(pending);
        pending = rest;
        await channelOperation(
          hostOrActive(hostId),
          "session.input",
          sessionId,
          {
            ch,
            seq: (seq += 1),
            b64: toBase64(new TextEncoder().encode(chunk)),
          },
          STREAM_LIFETIME_SECS,
        );
      }
    } catch (error) {
      pending = "";
      if (!closed) {
        onError(
          error instanceof HostControlError
            ? error
            : new HostControlError(
                (error as Error)?.message ?? "Input was not delivered.",
                "failed",
              ),
        );
      }
    } finally {
      busy = false;
    }
  }
  return {
    send(text) {
      if (closed || !text) return;
      pending += text;
      void flush();
    },
    close() {
      closed = true;
      pending = "";
    },
  };
}
