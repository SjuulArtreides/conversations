import type { CommunicationPage } from "./privateCommunications";
import type { AttentionIdentity, HostNotification } from "./privateAttention";
/**
 * Private browser <-> host channel: a WebRTC data channel, DTLS-encrypted end
 * to end. The server relays only the signaling (`peer_challenge`, `peer_offer`);
 * operations, history and search results travel on the channel and nowhere else.
 *
 * The answer is applied only after its host statement verifies against the
 * PINNED host key and binds exactly this offer, challenge, answer SDP and DTLS
 * fingerprint. The channel itself grants nothing: every message is a signed
 * operation the host checks against its current local grants, and every reply
 * is a host statement bound to this channel, this key and the exact request.
 *
 * There is no fallback. If the channel cannot be established or drops, callers
 * get `unavailable`; nothing is retried over the plaintext relay.
 * Wire types: `crates/core/src/private_channel.rs`.
 */

import type { AgentReadiness, Project, SessionLinearDoc } from "./types";
import {
  HOST_SCOPE,
  OP_LIFETIME_SECS,
  fromBase64Url,
  operationPayload,
  randomHex,
  signPayload,
  toBase64Url,
  type BrowserKey,
  type OperationName,
  type SignedFrame,
} from "./hostAuth";
import {
  checkLaunch,
  exactKeys,
  OP_STATUSES,
  OFFER_CLOCK_SKEW_SECS,
  StatementError,
  verifyStatement,
  type HostLaunchSettings,
  type OpStatus,
  type PinnedHost,
} from "./hostStatements";

export const CHANNEL_LABEL = "conversations-private/v1";
const MAX_SDP_BYTES = 12 * 1024;
const MAX_REQUEST_MESSAGE = 32 * 1024;
const MAX_REPLY_MESSAGE = 64 * 1024;
const MAX_REPLY_PAYLOAD = 44 * 1024;
/** Requests one channel keeps in flight; the host closes a channel that queues more. */
export const MAX_IN_FLIGHT = 4;
const RELAY_TIMEOUT_MS = 30_000;
const GATHER_TIMEOUT_MS = 5_000;
export const CONNECT_TIMEOUT_MS = 15_000;
const REPLY_TIMEOUT_MS = 30_000;
/** Raw bytes per transfer chunk (`MAX_CHUNK_BYTES` on the host). */
export const MAX_CHUNK_BYTES = 16 * 1024;
/** Unsent bytes this page lets the channel hold before it waits. */
const SEND_HIGH_WATER = 256 * 1024;
const MAX_FILES = 16;
const MAX_TOTAL_BYTES = 9 * 1024 * 1024;
/**
 * How long the host may take to report a transfer: its permit (at most the
 * operation's lifetime), the terminal's grace to stage and paste, and margin.
 */
const TRANSFER_OUTCOME_MS = (OP_LIFETIME_SECS + 30 + 30) * 1000;
/**
 * Terminal subscriptions are leases: each is a signed subscribe valid this long
 * by default, renewed at half of it with a fresh signed one.
 */
const TERMINAL_LEASE_SECS = OP_LIFETIME_SECS;
/** Shortest lease: a renewal at half of it must still have time to land. */
const MIN_LEASE_SECS = 2;
/** Largest snapshot or update this page reassembles from pieces. */
const MAX_TERMINAL_FRAME = 8 * 1024 * 1024;
/** Pieces held for a subscription whose reply is still being verified. */
const MAX_ORPHAN_PIECES = 64;

export type PrivateChannelErrorCode = "unavailable" | "untrusted" | "refused";

export class PrivateChannelError extends Error {
  code: PrivateChannelErrorCode;
  constructor(message: string, code: PrivateChannelErrorCode) {
    super(message);
    this.code = code;
  }
}

/**
 * One stored message: the complete canonical `Message` (blocks, tool calls and
 * results, usage) exactly as the host's local history holds it. `message` is
 * `null` on a digest page, or when the message is too large to travel inline;
 * then it is read in pieces with `historyFragment` and checked against `sha256`.
 */
export interface HistoryItem {
  seq: number;
  uuid: string;
  size: number;
  sha256: string;
  message: Record<string, unknown> | null;
}

export interface SearchHit {
  seq: number;
  uuid: string;
  snippet: string;
}

/** A local session the signer may read now. Private: only on the channel. */
export interface SessionEntry {
  id: string;
  agent: string;
  cwd: string;
  title: string | null;
  messages: number;
  last_activity: string | null;
  running: boolean;
  readiness?: AgentReadiness;
  prs?: string[];
  /** False when native continuation data is unavailable; absent on older hosts. */
  reopenable?: boolean;
  resume_mode?: "native" | "continuation";
}

export interface AccessCredential {
  kid: string;
  label: string;
  alg: string;
  kind: string;
  revoked: boolean;
  grants: { cap: string; session: string }[];
}

export interface MigrationManifest {
  agent: "claude" | "codex" | "opencode";
  native_id: string;
  title: string;
  size: number;
  sha256: string;
}

export type ReplyBody =
  | {
      kind: "migration";
      manifest: MigrationManifest;
      b64: string;
      next: number | null;
    }
  | CommunicationPage
  | { kind: "history"; items: HistoryItem[]; next: number | null }
  | {
      kind: "fragment";
      seq: number;
      offset: number;
      data: string;
      size: number;
      sha256: string;
      next: number | null;
    }
  | { kind: "search"; hits: SearchHit[] }
  | { kind: "sessions"; sessions: SessionEntry[]; next: string | null }
  | { kind: "settings"; launch: HostLaunchSettings }
  | { kind: "projects"; projects: Project[] }
  | { kind: "access"; credentials: AccessCredential[] }
  | { kind: "attention_recipients"; recipients: AttentionIdentity[] }
  | {
      kind: "notifications";
      notifications: HostNotification[];
      next: number | null;
    }
  | { kind: "linear"; docs: SessionLinearDoc[]; next: string | null }
  | { kind: "pr_cache"; b64: string; sha256: string; next: number | null }
  | { kind: "project_membership"; project_id: string | null }
  | { kind: "terminal"; sub: number };

/**
 * How the host settled a transfer. `delivered`: the terminal reported it staged
 * and pasted the paths (never Enter). `uncertain`: handed to the terminal, which
 * did not report back; it may or may not be pasted, and is never retried.
 */
export type TransferStatus =
  "delivered" | "refused" | "cancelled" | "failed" | "uncertain";
const TRANSFER_STATUSES: TransferStatus[] = [
  "delivered",
  "refused",
  "cancelled",
  "failed",
  "uncertain",
];

export interface TransferOptions {
  signal?: AbortSignal;
  onProgress?(sent: number, total: number): void;
}

export interface TerminalFrame {
  kind: "snapshot" | "update";
  cols: number;
  rows: number;
  data: Uint8Array;
}

/**
 * Why a followed terminal stopped:
 * - `closed`: this page closed it.
 * - `connection`: the private connection dropped.
 * - `expired`: its signed lease lapsed without a renewal. One fresh
 *   {@link PrivateChannel.subscribeTerminal} may follow; never extend or retry more.
 * - `denied`: the host refused a renewal or withdrew the authority it was
 *   admitted under. Needs a visible unlock; never retried automatically.
 * - `gone`: the terminal ended on the host.
 * - `protocol`: the stream broke its order or format.
 */
export type TerminalEndReason =
  "closed" | "connection" | "expired" | "denied" | "gone" | "protocol";

export interface TerminalEnd {
  reason: TerminalEndReason;
  /** Shown to the user; never terminal content. */
  detail: string;
}

export interface TerminalHandlers {
  onFrame(frame: TerminalFrame): void;
  onEnd(end: TerminalEnd): void;
}

export interface TerminalLease {
  /** Lifetime of each signed subscribe, whole seconds within the host's bounds. */
  leaseSecs?: number;
  /** `false`: never renew, so the lease lapses (tests of expiry). */
  autoRenew?: boolean;
}

export interface TerminalSubscription {
  sub: number;
  close(): void;
}

/** Largest single message this page reassembles from fragments. */
export const MAX_ASSEMBLED_MESSAGE = 8 * 1024 * 1024;

export interface ChannelResult {
  status: OpStatus;
  detail: string | null;
  body: ReplyBody | null;
}

export interface ChannelOptions {
  host: PinnedHost;
  key: BrowserKey;
  /** The delegation `key` acts under, or `null` for a directly approved key. */
  dlg: string | null;
  /** ICE servers from deployment configuration. None are built in. */
  iceServers?: RTCIceServer[];
  /** Use only TURN relay candidates (`iceTransportPolicy: "relay"`). */
  relayOnly?: boolean;
  connectTimeoutMs?: number;
}

export interface PeerChallenge {
  challenge: string;
  exp: number;
}

export interface SelectedPair {
  localType: string | null;
  remoteType: string | null;
  relayProtocol: string | null;
}

export interface PrivateChannel {
  /** The consumed challenge that names this channel in every host reply. */
  readonly peer: string;
  /** Sign and send one operation. */
  operation(
    op: OperationName,
    session: string,
    args?: Record<string, unknown>,
    timing?: { now?: number; lifetime?: number },
  ): Promise<ChannelResult>;
  /** Send an already signed frame as is (an exact retry keeps its bytes). */
  sendSigned(frame: SignedFrame): Promise<ChannelResult>;
  /** A page of history; `digest` returns only each message's size and hash. */
  historyGet(
    session: string,
    after?: number,
    limit?: number,
    digest?: boolean,
  ): Promise<ChannelResult>;
  historyFragment(
    session: string,
    seq: number,
    offset: number,
  ): Promise<ChannelResult>;
  historySearch(
    session: string,
    query: string,
    limit?: number,
  ): Promise<ChannelResult>;
  /** The host's sessions this key may read now (`after` is the page cursor). */
  sessionsList(
    after?: string | null,
    limit?: number,
    running_only?: boolean,
  ): Promise<ChannelResult>;
  settingsGet(): Promise<ChannelResult>;
  /**
   * Send files to a session's terminal, only on this channel: a signed
   * `session.send_files` naming each file's name, size and SHA-256, then the
   * bytes in order. The terminal pastes the staged paths; it never presses Enter.
   */
  sendFiles(
    session: string,
    files: { name: string; bytes: Uint8Array }[],
    opts?: TransferOptions,
  ): Promise<TransferStatus>;
  /** Paste an image into a session's terminal, like {@link sendFiles}. */
  pasteImage(
    session: string,
    mime: string,
    bytes: Uint8Array,
    opts?: TransferOptions,
  ): Promise<TransferStatus>;
  /** Follow a session's terminal: its current screen, then each update in order. */
  subscribeTerminal(
    session: string,
    handlers: TerminalHandlers,
    lease?: TerminalLease,
  ): Promise<TerminalSubscription>;
  selectedPair(): Promise<SelectedPair | null>;
  readonly open: boolean;
  close(): void;
}

type RelayReply =
  | { type: "peer_challenge"; statement: unknown }
  | { type: "peer_answer"; sdp: string; statement: unknown }
  | { type: "error"; message: string };

async function relay(
  hostId: string,
  request: Record<string, unknown>,
): Promise<RelayReply> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), RELAY_TIMEOUT_MS);
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
    throw new PrivateChannelError(
      "Could not reach the server to set up the private connection.",
      "unavailable",
    );
  } finally {
    clearTimeout(timer);
  }
  const body = (await response.json().catch(() => null)) as
    (RelayReply & { error?: string }) | null;
  if (!response.ok || !body || typeof body.type !== "string") {
    throw new PrivateChannelError(
      body?.error ?? `Your host is unavailable (${response.status}).`,
      "unavailable",
    );
  }
  return body;
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = new Uint8Array(
    await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>),
  );
  return Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join("");
}

async function sha256Text(text: string): Promise<string> {
  return sha256Bytes(new TextEncoder().encode(text));
}

async function sha256Bytes(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  return toBase64Url(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
  );
}

/** Same rule as the host: exactly one distinct `sha-256` fingerprint. */
export function sdpFingerprint(sdp: string): string {
  let found: string | null = null;
  for (const raw of sdp.split("\n")) {
    const line = raw.replace(/\r$/, "");
    if (!line.startsWith("a=fingerprint:")) continue;
    const parts = line.slice("a=fingerprint:".length).split(" ");
    if (parts.length !== 2 || parts[0].toLowerCase() !== "sha-256") {
      throw new StatementError(
        "The connection offer has an unusable fingerprint.",
      );
    }
    const value = parts[1].toUpperCase();
    if (!/^([0-9A-F]{2}:){31}[0-9A-F]{2}$/.test(value)) {
      throw new StatementError(
        "The connection offer has an unusable fingerprint.",
      );
    }
    const fingerprint = `sha-256 ${value}`;
    if (found !== null && found !== fingerprint) {
      throw new StatementError("The connection has conflicting fingerprints.");
    }
    found = fingerprint;
  }
  if (found === null) {
    throw new StatementError("The connection has no fingerprint.");
  }
  return found;
}

function untrusted(error: unknown): never {
  if (error instanceof PrivateChannelError) throw error;
  throw new PrivateChannelError(
    error instanceof Error ? error.message : String(error),
    "untrusted",
  );
}

/** Ask the host for a fresh one-use challenge, verified against the pin. */
export async function requestPeerChallenge(
  host: PinnedHost,
): Promise<PeerChallenge> {
  const nonce = randomHex(16);
  const reply = await relay(host.id, { type: "peer_challenge", nonce });
  if (reply.type === "error") {
    throw new PrivateChannelError(reply.message, "refused");
  }
  if (reply.type !== "peer_challenge") {
    untrusted(new StatementError("The host sent an unexpected answer."));
  }
  try {
    const { payload } = await verifyStatement(host, reply.statement);
    exactKeys(payload, ["v", "typ", "host", "nonce", "challenge", "exp"]);
    const now = Math.floor(Date.now() / 1000);
    if (
      payload.typ !== "peer_challenge" ||
      payload.nonce !== nonce ||
      typeof payload.challenge !== "string" ||
      !/^[0-9a-f]{32}$/.test(payload.challenge) ||
      typeof payload.exp !== "number" ||
      payload.exp < now - OFFER_CLOCK_SKEW_SECS
    ) {
      throw new StatementError(
        "The host's challenge does not match this request.",
      );
    }
    return { challenge: payload.challenge, exp: payload.exp };
  } catch (error) {
    return untrusted(error);
  }
}

async function gatheredOffer(pc: RTCPeerConnection): Promise<string> {
  await pc.setLocalDescription(await pc.createOffer());
  await new Promise<void>((resolve) => {
    if (pc.iceGatheringState === "complete") return resolve();
    const timer = setTimeout(resolve, GATHER_TIMEOUT_MS);
    pc.addEventListener("icegatheringstatechange", () => {
      if (pc.iceGatheringState === "complete") {
        clearTimeout(timer);
        resolve();
      }
    });
  });
  const sdp = pc.localDescription?.sdp;
  if (!sdp)
    throw new PrivateChannelError("No connection offer.", "unavailable");
  return sdp;
}

/** Open a private channel with a fresh challenge. */
export async function openPrivateChannel(
  options: ChannelOptions,
): Promise<PrivateChannel> {
  return connectWithChallenge(
    options,
    await requestPeerChallenge(options.host),
  );
}

/** Open a private channel using an already issued challenge (used once). */
export async function connectWithChallenge(
  options: ChannelOptions,
  challenge: PeerChallenge,
): Promise<PrivateChannel> {
  const { host, key, dlg } = options;
  const pc = new RTCPeerConnection({
    iceServers: options.iceServers ?? [],
    iceTransportPolicy: options.relayOnly ? "relay" : "all",
  });
  try {
    const dc = pc.createDataChannel(CHANNEL_LABEL, { ordered: true });
    const opened = new Promise<boolean>((resolve) => {
      dc.addEventListener("open", () => resolve(true));
      dc.addEventListener("close", () => resolve(false));
    });
    const sdp = await gatheredOffer(pc);
    if (new TextEncoder().encode(sdp).length > MAX_SDP_BYTES) {
      throw new PrivateChannelError(
        "The connection offer is too large.",
        "unavailable",
      );
    }
    const fingerprint = sdpFingerprint(sdp);
    const payload = operationPayload({
      hostId: host.id,
      key,
      dlg,
      op: "peer.offer",
      session: HOST_SCOPE,
      args: {
        challenge: challenge.challenge,
        sdp_sha256: await sha256Text(sdp),
        fingerprint,
      },
    });
    const frame = await signPayload(key, payload);
    const reply = await relay(host.id, { type: "peer_offer", ...frame, sdp });
    if (reply.type === "error") {
      throw new PrivateChannelError(reply.message, "refused");
    }
    if (reply.type !== "peer_answer" || typeof reply.sdp !== "string") {
      throw new PrivateChannelError(
        "The host sent an unexpected answer.",
        "untrusted",
      );
    }
    // Everything about the answer is checked against the pin BEFORE it is applied.
    try {
      const { payload: answer } = await verifyStatement(host, reply.statement);
      exactKeys(answer, [
        "v",
        "typ",
        "host",
        "kid",
        "dlg",
        "challenge",
        "offer",
        "answer",
        "fingerprint",
      ]);
      if (
        answer.typ !== "peer_answer" ||
        answer.kid !== key.kid ||
        answer.dlg !== dlg ||
        answer.challenge !== challenge.challenge ||
        answer.offer !== (await sha256Bytes(fromBase64Url(frame.p))) ||
        reply.sdp.length > MAX_SDP_BYTES ||
        answer.answer !== (await sha256Text(reply.sdp)) ||
        answer.fingerprint !== sdpFingerprint(reply.sdp)
      ) {
        throw new StatementError(
          "The host's answer does not match this connection request.",
        );
      }
    } catch (error) {
      untrusted(error);
    }
    await pc.setRemoteDescription({ type: "answer", sdp: reply.sdp });
    const timeout = options.connectTimeoutMs ?? CONNECT_TIMEOUT_MS;
    const ok = await Promise.race([
      opened,
      new Promise<boolean>((resolve) =>
        setTimeout(() => resolve(false), timeout),
      ),
    ]);
    if (!ok || dc.readyState !== "open") {
      throw new PrivateChannelError(
        "Your host could not be reached privately. Nothing was sent.",
        "unavailable",
      );
    }
    return channel(pc, dc, options, challenge.challenge);
  } catch (error) {
    pc.close();
    throw error;
  }
}

/**
 * The channel over an open data channel whose peer was verified. Exported for
 * tests only; callers use {@link openPrivateChannel}.
 */
export function channel(
  pc: Pick<RTCPeerConnection, "close" | "getStats">,
  dc: RTCDataChannel,
  options: Pick<ChannelOptions, "host" | "key" | "dlg">,
  peer: string,
): PrivateChannel {
  const { host, key, dlg } = options;
  let nextId = 1;
  const pending = new Map<
    number,
    {
      request: string;
      resolve: (result: ChannelResult) => void;
      reject: (error: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();
  const failAll = (error: PrivateChannelError) => {
    for (const [id, entry] of pending) {
      clearTimeout(entry.timer);
      pending.delete(id);
      entry.reject(error);
    }
  };
  // The host closes the channel if more than MAX_IN_FLIGHT requests queue on
  // it at once, so this page must never put more than that many on the wire.
  // A per-request admission slot is reserved before anything is awaited or
  // sent, and released when the reply, timeout, failAll or close settles
  // that request. Callers wait in order for a free slot: concurrent sends
  // (metadata reads + a keystroke on tab switch) queue instead of racing
  // past the limit and blowing the connection.
  let inFlight = 0;
  const admissionWaiters: Array<{
    acquire: () => void;
    reject: (error: PrivateChannelError) => void;
  }> = [];
  const acquireSlot = (): Promise<void> =>
    new Promise((resolve, reject) => {
      if (closed) {
        reject(
          new PrivateChannelError(
            "The private connection is closed.",
            "unavailable",
          ),
        );
        return;
      }
      if (inFlight < MAX_IN_FLIGHT) {
        inFlight += 1;
        resolve();
        return;
      }
      admissionWaiters.push({
        acquire: () => {
          inFlight += 1;
          resolve();
        },
        reject,
      });
    });
  const releaseSlot = () => {
    inFlight -= 1;
    // Once the channel is closing, `close()` fails every waiter directly:
    // waking one here would hand it a slot that cannot be used.
    if (closed) return;
    const next = admissionWaiters.shift();
    if (next) next.acquire();
  };
  /** Transfers waiting for the host's outcome, by transfer id. */
  const transfers = new Map<string, (status: TransferStatus) => void>();
  /** Followed terminals, by subscription id. */
  const subs = new Map<number, Following>();
  /** Pieces that arrived before their subscribe reply was verified. */
  const orphans: TerminalPiece[] = [];
  /** Subscriptions this page told the host to stop. */
  const left = new Set<number>();
  let pendingSubscribes = 0;
  /**
   * Sessions this channel follows or is subscribing to. The host treats a
   * second subscribe as a renewal, so it is never signed while one is live.
   */
  const followed = new Set<string>();
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    failAll(
      new PrivateChannelError(
        "The private connection closed. The request may or may not have run; it was not retried.",
        "unavailable",
      ),
    );
    // Any caller waiting for a free admission slot must be released now:
    // the channel will not reopen, so they would wait forever.
    while (admissionWaiters.length) {
      admissionWaiters
        .shift()!
        .reject(
          new PrivateChannelError(
            "The private connection closed. The request may or may not have run; it was not retried.",
            "unavailable",
          ),
        );
    }
    for (const [id, settle] of transfers) {
      transfers.delete(id);
      settle("uncertain");
    }
    for (const [sub, following] of subs) {
      subs.delete(sub);
      following.end({
        reason: "connection",
        detail: "The private connection closed.",
      });
    }
    dc.close();
    pc.close();
  };
  const unsubscribe = (sub: number) => {
    if (dc.readyState === "open") {
      dc.send(JSON.stringify({ t: "unsubscribe", sub }));
    }
  };
  const onTerminal = (piece: TerminalPiece) => {
    const following = subs.get(piece.sub);
    if (!following) {
      // Its reply may still be verifying; anything else is a late piece of a
      // subscription this page already left.
      if (pendingSubscribes > 0) {
        if (orphans.length >= MAX_ORPHAN_PIECES) return close();
        orphans.push(piece);
      } else if (piece.kind !== "end" && !left.has(piece.sub)) {
        left.add(piece.sub);
        unsubscribe(piece.sub);
      }
      return;
    }
    const ended = following.take(piece);
    if (ended !== null) {
      subs.delete(piece.sub);
      if (!ended.byHost) unsubscribe(piece.sub);
      following.end(ended.end);
    }
  };
  dc.addEventListener("close", close);
  dc.addEventListener("message", (event: MessageEvent) => {
    void (async () => {
      const text = typeof event.data === "string" ? event.data : null;
      if (text === null || text.length > MAX_REPLY_MESSAGE) return close();
      let message: { t?: unknown; id?: unknown; statement?: unknown };
      try {
        message = JSON.parse(text);
      } catch {
        return close();
      }
      if (message.t === "transfer") {
        const outcome = transferMessage(message);
        if (!outcome) return close();
        const settle = transfers.get(outcome.transfer);
        if (settle) {
          transfers.delete(outcome.transfer);
          settle(outcome.status);
        }
        return;
      }
      if (message.t === "terminal") {
        const piece = terminalMessage(message);
        if (!piece) return close();
        return onTerminal(piece);
      }
      if (
        message.t !== "reply" ||
        typeof message.id !== "number" ||
        Object.keys(message).sort().join() !== "id,statement,t"
      ) {
        return close();
      }
      const entry = pending.get(message.id);
      if (!entry) return;
      pending.delete(message.id);
      clearTimeout(entry.timer);
      try {
        entry.resolve(
          await checkReply(
            host,
            peer,
            key.kid,
            entry.request,
            message.statement,
          ),
        );
      } catch (error) {
        entry.reject(
          new PrivateChannelError(
            error instanceof Error ? error.message : String(error),
            "untrusted",
          ),
        );
        close();
      }
    })();
  });

  const sendSigned = async (frame: SignedFrame): Promise<ChannelResult> => {
    if (dc.readyState !== "open") {
      throw new PrivateChannelError(
        "The private connection is closed.",
        "unavailable",
      );
    }
    // Reserve an admission slot before any work that could await (hashing)
    // or that touches the wire (`dc.send`). Concurrent callers above the
    // host's cap wait in order for a free slot instead of all passing a
    // `pending.size` check and then racing past it after the hash await.
    await acquireSlot();
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      releaseSlot();
    };
    try {
      if (dc.readyState !== "open") {
        throw new PrivateChannelError(
          "The private connection is closed.",
          "unavailable",
        );
      }
      const id = nextId++;
      const body = JSON.stringify({ t: "op", id, ...frame });
      if (body.length > MAX_REQUEST_MESSAGE) {
        throw new PrivateChannelError("The request is too large.", "refused");
      }
      const request = await sha256Bytes(fromBase64Url(frame.p));
      return await new Promise<ChannelResult>((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          release();
          reject(
            new PrivateChannelError(
              "Your host did not answer. The request may or may not have run; it was not retried.",
              "unavailable",
            ),
          );
        }, REPLY_TIMEOUT_MS);
        pending.set(id, {
          request,
          resolve: (result) => {
            release();
            resolve(result);
          },
          reject: (error) => {
            release();
            reject(error);
          },
          timer,
        });
        dc.send(body);
      });
    } catch (error) {
      release();
      throw error;
    }
  };

  /** Wait until the channel can take more bytes. */
  const drained = () =>
    new Promise<void>((resolve) => {
      if (
        dc.bufferedAmount <= SEND_HIGH_WATER / 2 ||
        dc.readyState !== "open"
      ) {
        return resolve();
      }
      dc.bufferedAmountLowThreshold = SEND_HIGH_WATER / 2;
      const done = () => {
        dc.removeEventListener("bufferedamountlow", done);
        dc.removeEventListener("close", done);
        resolve();
      };
      dc.addEventListener("bufferedamountlow", done);
      dc.addEventListener("close", done);
    });

  /**
   * Sign `op` naming exactly `files`, then stream their bytes. Nothing here,
   * on any outcome, sends a byte anywhere but this channel.
   */
  const transfer = async (
    session: string,
    op: "session.send_files" | "session.paste_image",
    args: (transfer: string) => Promise<Record<string, unknown>>,
    files: Uint8Array[],
    opts: TransferOptions = {},
  ): Promise<TransferStatus> => {
    const id = randomHex(16);
    const total = files.reduce((sum, f) => sum + f.length, 0);
    if (opts.signal?.aborted) return "cancelled";
    // Waiting before the operation is sent: the outcome can follow its reply at once.
    let settle!: (status: TransferStatus) => void;
    const outcome = new Promise<TransferStatus>((resolve) => {
      settle = resolve;
    });
    transfers.set(id, settle);
    let result: ChannelResult;
    try {
      result = await operation(op, session, await args(id));
    } catch (error) {
      transfers.delete(id);
      throw error;
    }
    if (result.status !== "executed") {
      transfers.delete(id);
      throw new PrivateChannelError(
        result.detail ??
          "Your host did not accept this transfer. Nothing was sent.",
        "refused",
      );
    }
    let cancelled = false;
    const cancel = () => {
      if (cancelled || !transfers.has(id)) return;
      cancelled = true;
      if (dc.readyState === "open") {
        dc.send(JSON.stringify({ t: "cancel", transfer: id }));
      }
    };
    opts.signal?.addEventListener("abort", cancel, { once: true });
    try {
      let sent = 0;
      for (let file = 0; file < files.length && !cancelled; file++) {
        const bytes = files[file];
        for (
          let offset = 0;
          offset < bytes.length && !cancelled;
          offset += MAX_CHUNK_BYTES
        ) {
          await drained();
          if (dc.readyState !== "open" || !transfers.has(id)) break;
          if (cancelled) break;
          const piece = bytes.subarray(offset, offset + MAX_CHUNK_BYTES);
          dc.send(
            JSON.stringify({
              t: "chunk",
              transfer: id,
              file,
              offset,
              data: toBase64Url(piece),
            }),
          );
          sent += piece.length;
          opts.onProgress?.(sent, total);
        }
      }
      if (sent < total && !cancelled) {
        // Stopped before every byte left: the host refused it already, or the
        // channel closed and the host dropped what it received, applying nothing.
        if (transfers.has(id)) {
          transfers.delete(id);
          return "failed";
        }
        // The host's own status stands; only our closed channel's placeholder
        // becomes `failed`, since the host never had every byte to apply.
        const status = await outcome;
        return status === "uncertain" && closed ? "failed" : status;
      }
      const timer = new Promise<TransferStatus>((resolve) =>
        setTimeout(() => resolve("uncertain"), TRANSFER_OUTCOME_MS),
      );
      const status = await Promise.race([outcome, timer]);
      transfers.delete(id);
      return status;
    } finally {
      opts.signal?.removeEventListener("abort", cancel);
    }
  };

  const sendFiles: PrivateChannel["sendFiles"] = async (
    session,
    files,
    opts,
  ) => {
    const total = files.reduce((sum, f) => sum + f.bytes.length, 0);
    if (
      files.length === 0 ||
      files.length > MAX_FILES ||
      total > MAX_TOTAL_BYTES
    ) {
      throw new PrivateChannelError("Too many files, or too large.", "refused");
    }
    return transfer(
      session,
      "session.send_files",
      async (id) => ({
        transfer: id,
        files: await Promise.all(
          files.map(async (f) => ({
            name: f.name,
            size: f.bytes.length,
            sha256: await sha256Hex(f.bytes),
          })),
        ),
      }),
      files.map((f) => f.bytes),
      opts,
    );
  };

  const pasteImage: PrivateChannel["pasteImage"] = async (
    session,
    mime,
    bytes,
    opts,
  ) => {
    if (bytes.length === 0 || bytes.length > MAX_TOTAL_BYTES) {
      throw new PrivateChannelError(
        "The image is empty or too large.",
        "refused",
      );
    }
    return transfer(
      session,
      "session.paste_image",
      async (id) => ({
        transfer: id,
        mime,
        size: bytes.length,
        sha256: await sha256Hex(bytes),
      }),
      [bytes],
      opts,
    );
  };

  /** Sign a subscribe; the verified reply names the subscription. */
  const subscribeOnce = async (
    session: string,
    leaseSecs: number,
  ): Promise<number> => {
    const result = await operation(
      "terminal.subscribe",
      session,
      {},
      { lifetime: leaseSecs },
    );
    if (result.status !== "executed" || result.body?.kind !== "terminal") {
      throw new PrivateChannelError(
        result.detail ?? "Your host did not open this terminal.",
        "refused",
      );
    }
    return result.body.sub;
  };

  const subscribeTerminal: PrivateChannel["subscribeTerminal"] = async (
    session,
    handlers,
    lease = {},
  ) => {
    const leaseSecs = lease.leaseSecs ?? TERMINAL_LEASE_SECS;
    if (
      !Number.isInteger(leaseSecs) ||
      leaseSecs < MIN_LEASE_SECS ||
      leaseSecs > TERMINAL_LEASE_SECS
    ) {
      throw new PrivateChannelError(
        `A terminal lease must be ${MIN_LEASE_SECS} to ${TERMINAL_LEASE_SECS} whole seconds.`,
        "refused",
      );
    }
    if (followed.has(session)) {
      // One follower per terminal on a channel.
      throw new PrivateChannelError(
        "This terminal is already followed on this connection.",
        "refused",
      );
    }
    followed.add(session);
    pendingSubscribes += 1;
    let sub: number;
    try {
      sub = await subscribeOnce(session, leaseSecs);
    } catch (error) {
      followed.delete(session);
      throw error;
    } finally {
      pendingSubscribes -= 1;
    }
    if (subs.has(sub)) {
      followed.delete(session);
      throw new PrivateChannelError(
        "Your host answered with a terminal stream this page already follows.",
        "untrusted",
      );
    }
    let renew: ReturnType<typeof setTimeout> | undefined;
    let ended = false;
    const following = new Following(sub, handlers.onFrame, (end) => {
      if (ended) return;
      ended = true;
      followed.delete(session);
      clearTimeout(renew);
      handlers.onEnd(end);
    });
    subs.set(sub, following);
    const leave = (end: TerminalEnd) => {
      if (subs.get(sub) !== following) return;
      subs.delete(sub);
      unsubscribe(sub);
      following.end(end);
    };
    // The host releases each frame under the subscribe it last admitted, and
    // stops at that operation's expiry: renew it with a fresh signed one.
    const schedule = () => {
      renew = setTimeout(
        async () => {
          try {
            const renewed = await subscribeOnce(session, leaseSecs);
            if (subs.get(sub) !== following) {
              // Ended meanwhile: the renewal must not leave the host streaming.
              if (renewed === sub) unsubscribe(sub);
              return;
            }
            if (renewed !== sub) {
              return leave({
                reason: "protocol",
                detail: "Your host renewed another terminal stream.",
              });
            }
            schedule();
          } catch (error) {
            const detail =
              error instanceof Error ? error.message : String(error);
            const code =
              error instanceof PrivateChannelError ? error.code : null;
            leave({
              reason:
                code === "refused"
                  ? "denied"
                  : code === "untrusted"
                    ? "protocol"
                    : "connection",
              detail,
            });
          }
        },
        (leaseSecs * 1000) / 2,
      );
    };
    if (lease.autoRenew !== false) schedule();
    for (const piece of orphans.splice(0)) {
      if (piece.sub === sub) onTerminal(piece);
      else if (pendingSubscribes > 0) orphans.push(piece);
    }
    return {
      sub,
      close: () => leave({ reason: "closed", detail: "Closed." }),
    };
  };

  const operation: PrivateChannel["operation"] = async (
    op,
    session,
    args,
    timing,
  ) =>
    sendSigned(
      await signPayload(
        key,
        operationPayload({
          hostId: host.id,
          key,
          dlg,
          op,
          session,
          args,
          now: timing?.now,
          lifetime: timing?.lifetime,
        }),
      ),
    );

  return {
    peer,
    operation,
    sendSigned,
    historyGet: (session, after = 0, limit = 32, digest = false) =>
      operation("history.get", session, {
        session,
        after,
        limit,
        ...(digest ? { digest: true } : {}),
      }),
    historyFragment: (session, seq, offset) =>
      operation("history.fragment", session, { session, seq, offset }),
    historySearch: (session, query, limit = 10) =>
      operation("history.search", session, { session, query, limit }),
    sessionsList: sessionListing(operation),
    settingsGet: () => operation("settings.get", HOST_SCOPE, {}),
    sendFiles,
    pasteImage,
    subscribeTerminal,
    selectedPair: () => selectedPair(pc),
    get open() {
      return dc.readyState === "open";
    },
    close,
  };
}

/** Cached session identity can survive an outage. Readiness evidence cannot. */
export function withoutReadiness(sessions: SessionEntry[]): SessionEntry[] {
  return sessions.map((session) => ({ ...session, readiness: "unknown" }));
}

/** New metadata is opt-in because older browsers reject extra session fields. */
export function sessionListing(
  operation: PrivateChannel["operation"],
): PrivateChannel["sessionsList"] {
  let readiness = true;
  return async (after = null, limit = 50, running_only = false) => {
    const args = { after, limit, ...(running_only ? { running_only } : {}) };
    const answer = await operation("sessions.list", HOST_SCOPE, {
      ...args,
      ...(readiness ? { readiness: true } : {}),
    });
    if (
      readiness &&
      answer.status === "refused" &&
      answer.detail?.startsWith(
        "malformed operation: unknown field `readiness`",
      )
    ) {
      // The old host cannot parse the additive request. Retry this read only.
      readiness = false;
      return operation("sessions.list", HOST_SCOPE, args);
    }
    return answer;
  };
}

async function checkReply(
  host: PinnedHost,
  peer: string,
  kid: string,
  request: string,
  statement: unknown,
): Promise<ChannelResult> {
  const { payload } = await verifyStatement(host, statement, MAX_REPLY_PAYLOAD);
  exactKeys(payload, [
    "v",
    "typ",
    "host",
    "peer",
    "kid",
    "request",
    "status",
    "detail",
    "body",
  ]);
  if (payload.typ !== "channel_reply")
    throw new StatementError("Wrong reply type.");
  if (payload.peer !== peer)
    throw new StatementError("Reply for another connection.");
  if (payload.kid !== kid) throw new StatementError("Reply for another key.");
  if (payload.request !== request)
    throw new StatementError("Reply to another request.");
  if (!OP_STATUSES.includes(payload.status as OpStatus)) {
    throw new StatementError("Unknown status.");
  }
  if (payload.detail !== null && typeof payload.detail !== "string") {
    throw new StatementError("Malformed detail.");
  }
  return {
    status: payload.status as OpStatus,
    detail: payload.detail as string | null,
    body: checkBody(payload.body),
  };
}

export function checkBody(body: unknown): ReplyBody | null {
  if (body === null) return null;
  const bad = () => new StatementError("Malformed reply content.");
  if (typeof body !== "object" || Array.isArray(body)) throw bad();
  const b = body as Record<string, unknown>;
  const count = (v: unknown) => Number.isSafeInteger(v) && (v as number) >= 0;
  const record = (v: unknown): Record<string, unknown> => {
    if (typeof v !== "object" || v === null || Array.isArray(v)) throw bad();
    return v as Record<string, unknown>;
  };
  if (b.kind === "history") {
    exactKeys(b, ["kind", "items", "next"]);
    if (!Array.isArray(b.items) || (b.next !== null && !count(b.next))) {
      throw bad();
    }
    for (const raw of b.items) {
      const item = record(raw);
      exactKeys(item, ["seq", "uuid", "size", "sha256", "message"]);
      if (
        !count(item.seq) ||
        typeof item.uuid !== "string" ||
        !count(item.size) ||
        typeof item.sha256 !== "string" ||
        (item.message !== null && record(item.message).uuid !== item.uuid)
      ) {
        throw bad();
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "fragment") {
    exactKeys(b, ["kind", "seq", "offset", "data", "size", "sha256", "next"]);
    if (
      !count(b.seq) ||
      !count(b.offset) ||
      typeof b.data !== "string" ||
      !count(b.size) ||
      typeof b.sha256 !== "string" ||
      (b.next !== null && !count(b.next))
    ) {
      throw bad();
    }
    return b as ReplyBody;
  }
  if (b.kind === "search") {
    exactKeys(b, ["kind", "hits"]);
    if (!Array.isArray(b.hits)) throw bad();
    for (const raw of b.hits) {
      const hit = record(raw);
      exactKeys(hit, ["seq", "uuid", "snippet"]);
      if (
        !count(hit.seq) ||
        typeof hit.uuid !== "string" ||
        typeof hit.snippet !== "string"
      ) {
        throw bad();
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "sessions") {
    exactKeys(b, ["kind", "sessions", "next"]);
    if (
      !Array.isArray(b.sessions) ||
      (b.next !== null && typeof b.next !== "string")
    ) {
      throw bad();
    }
    for (const raw of b.sessions) {
      const s = record(raw);
      exactKeys(
        {
          ...s,
          prs: s.prs ?? [],
          reopenable: s.reopenable,
          resume_mode: s.resume_mode,
          readiness: s.readiness,
        },
        [
          "id",
          "agent",
          "cwd",
          "title",
          "messages",
          "last_activity",
          "running",
          "prs",
          "reopenable",
          "resume_mode",
          "readiness",
        ],
      );
      if (
        typeof s.id !== "string" ||
        typeof s.agent !== "string" ||
        typeof s.cwd !== "string" ||
        (s.title !== null && typeof s.title !== "string") ||
        !count(s.messages) ||
        (s.last_activity !== null && typeof s.last_activity !== "string") ||
        typeof s.running !== "boolean" ||
        (s.readiness !== undefined &&
          ![
            "unknown",
            "working",
            "background",
            "needs_input",
            "ready",
          ].includes(s.readiness as string)) ||
        (s.reopenable !== undefined && typeof s.reopenable !== "boolean") ||
        (s.resume_mode !== undefined &&
          s.resume_mode !== "native" &&
          s.resume_mode !== "continuation") ||
        (s.prs !== undefined &&
          (!Array.isArray(s.prs) ||
            s.prs.length > 100 ||
            !s.prs.every(
              (url) =>
                typeof url === "string" &&
                /^https:\/\/github\.com\/[^/\s]+\/[^/\s]+\/pull\/[1-9][0-9]*$/.test(
                  url,
                ),
            )))
      ) {
        throw bad();
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "terminal") {
    exactKeys(b, ["kind", "sub"]);
    if (!count(b.sub)) throw bad();
    return b as ReplyBody;
  }
  if (b.kind === "communications") {
    exactKeys(b, ["kind", "peers", "exchanges", "next"]);
    if (
      !Array.isArray(b.peers) ||
      !Array.isArray(b.exchanges) ||
      (b.next !== null && typeof b.next !== "string")
    )
      throw bad();
    const nullableString = (v: unknown) => v === null || typeof v === "string";
    for (const raw of b.peers) {
      const p = record(raw);
      exactKeys(p, [
        "session_id",
        "host_id",
        "title",
        "agent",
        "last_seen_at",
        "connected",
        "state",
        "reachable",
        "readable",
      ]);
      if (
        typeof p.session_id !== "string" ||
        typeof p.title !== "string" ||
        ![p.host_id, p.agent, p.last_seen_at].every(nullableString) ||
        !["live", "ended", "unknown"].includes(p.state as string) ||
        ![p.connected, p.reachable].every(
          (v) => v === null || typeof v === "boolean",
        ) ||
        typeof p.readable !== "boolean"
      )
        throw bad();
    }
    for (const raw of b.exchanges) {
      const e = record(raw);
      exactKeys(e, [
        "id",
        "peer_session_id",
        "host_id",
        "direction",
        "title",
        "status",
        "created_at",
      ]);
      if (
        ![e.id, e.peer_session_id, e.title, e.status].every(
          (v) => typeof v === "string",
        ) ||
        ![e.host_id, e.created_at].every(nullableString) ||
        !["sent", "received"].includes(e.direction as string)
      )
        throw bad();
    }
    return b as ReplyBody;
  }
  if (b.kind === "attention_recipients" || b.kind === "notifications") {
    const subject = (value: unknown) => {
      const s = record(value);
      if (s.kind === "local_owner") exactKeys(s, ["kind"]);
      else {
        exactKeys(s, ["kind", "kid"]);
        if (
          s.kind !== "credential" ||
          typeof s.kid !== "string" ||
          !/^[A-Za-z0-9_-]{43}$/.test(s.kid)
        )
          throw bad();
      }
    };
    const identity = (value: unknown) => {
      const i = record(value);
      exactKeys(i, ["subject", "label"]);
      if (typeof i.label !== "string") throw bad();
      subject(i.subject);
    };
    if (b.kind === "attention_recipients") {
      exactKeys(b, ["kind", "recipients"]);
      if (!Array.isArray(b.recipients)) throw bad();
      b.recipients.forEach(identity);
    } else {
      exactKeys(b, ["kind", "notifications", "next"]);
      if (
        !Array.isArray(b.notifications) ||
        (b.next !== null && !count(b.next))
      )
        throw bad();
      for (const raw of b.notifications) {
        const n = record(raw);
        exactKeys(n, [
          "id",
          "session_id",
          "session_title",
          "kind",
          "sender",
          "recipient",
          "message",
          "created_at",
          "read_at",
          "resolved_at",
        ]);
        if (
          n.kind !== "attention" ||
          ![n.id, n.session_id, n.message, n.created_at].every(
            (v) => typeof v === "string",
          ) ||
          ![n.session_title, n.read_at, n.resolved_at].every(
            (v) => v === null || typeof v === "string",
          )
        )
          throw bad();
        identity(n.sender);
        subject(n.recipient);
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "linear") {
    exactKeys(b, ["kind", "docs", "next"]);
    if (
      !Array.isArray(b.docs) ||
      (b.next !== null && typeof b.next !== "string")
    )
      throw bad();
    for (const raw of b.docs) {
      const doc = record(raw);
      exactKeys(doc, ["url", "workspace", "kind", "identifier"]);
      if (
        ![doc.url, doc.workspace, doc.kind, doc.identifier].every(
          (value) => typeof value === "string",
        ) ||
        !["document", "issue", "project"].includes(doc.kind as string)
      )
        throw bad();
      try {
        const url = new URL(doc.url as string);
        if (url.protocol !== "https:" || url.hostname !== "linear.app")
          throw bad();
      } catch {
        throw bad();
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "migration") {
    exactKeys(b, ["kind", "manifest", "b64", "next"]);
    const m = record(b.manifest);
    exactKeys(m, ["agent", "native_id", "title", "size", "sha256"]);
    if (
      !["claude", "codex", "opencode"].includes(m.agent as string) ||
      typeof m.native_id !== "string" ||
      !/^[A-Za-z0-9_-]{1,128}$/.test(m.native_id) ||
      typeof m.title !== "string" ||
      m.title.length > 1024 ||
      !count(m.size) ||
      (m.size as number) < 1 ||
      (m.size as number) > 64 * 1024 * 1024 ||
      typeof m.sha256 !== "string" ||
      !/^[A-Za-z0-9_-]{43}$/.test(m.sha256) ||
      typeof b.b64 !== "string" ||
      b.b64.length > 8192 ||
      !/^[A-Za-z0-9_-]+$/.test(b.b64) ||
      (b.next !== null &&
        (!count(b.next) || (b.next as number) >= (m.size as number)))
    )
      throw bad();
    return b as ReplyBody;
  }
  if (b.kind === "pr_cache") {
    exactKeys(b, ["kind", "b64", "sha256", "next"]);
    if (
      typeof b.b64 !== "string" ||
      typeof b.sha256 !== "string" ||
      !/^[A-Za-z0-9_-]{43}$/.test(b.sha256) ||
      (b.next !== null && !count(b.next))
    )
      throw bad();
    return b as ReplyBody;
  }
  if (b.kind === "project_membership") {
    exactKeys(b, ["kind", "project_id"]);
    if (b.project_id !== null && typeof b.project_id !== "string") throw bad();
    return b as ReplyBody;
  }
  if (b.kind === "access") {
    exactKeys(b, ["kind", "credentials"]);
    if (!Array.isArray(b.credentials)) throw bad();
    for (const raw of b.credentials) {
      const credential = record(raw);
      exactKeys(credential, [
        "kid",
        "label",
        "alg",
        "kind",
        "revoked",
        "grants",
      ]);
      if (
        ![
          credential.kid,
          credential.label,
          credential.alg,
          credential.kind,
        ].every((value) => typeof value === "string") ||
        !/^[A-Za-z0-9_-]{43}$/.test(credential.kid as string) ||
        typeof credential.revoked !== "boolean" ||
        !Array.isArray(credential.grants)
      )
        throw bad();
      for (const rawGrant of credential.grants) {
        const grant = record(rawGrant);
        exactKeys(grant, ["cap", "session"]);
        if (typeof grant.cap !== "string" || typeof grant.session !== "string")
          throw bad();
      }
    }
    return b as ReplyBody;
  }
  if (b.kind === "projects") {
    exactKeys(b, ["kind", "projects"]);
    if (!Array.isArray(b.projects)) throw bad();
    for (const raw of b.projects) {
      const project = record(raw);
      exactKeys(project, [
        "id",
        "name",
        "description",
        "owner_email",
        "session_count",
        "created_at",
        "updated_at",
      ]);
      if (
        ![
          project.id,
          project.name,
          project.created_at,
          project.updated_at,
        ].every((value) => typeof value === "string") ||
        !count(project.session_count) ||
        (project.description !== null &&
          typeof project.description !== "string") ||
        (project.owner_email !== null &&
          typeof project.owner_email !== "string")
      )
        throw bad();
    }
    return b as ReplyBody;
  }
  if (b.kind === "settings") {
    exactKeys(b, ["kind", "launch"]);
    return { kind: "settings", launch: checkLaunch(b.launch) };
  }
  throw bad();
}

function transferMessage(
  message: Record<string, unknown>,
): { transfer: string; status: TransferStatus } | null {
  if (
    Object.keys(message).sort().join() !== "status,t,transfer" ||
    typeof message.transfer !== "string" ||
    !TRANSFER_STATUSES.includes(message.status as TransferStatus)
  ) {
    return null;
  }
  return {
    transfer: message.transfer,
    status: message.status as TransferStatus,
  };
}

/** Why the host ended a subscription (only on an `end` piece). */
type HostEnd = "expired" | "withdrawn" | "gone" | "unsubscribed";
const HOST_ENDS: HostEnd[] = ["expired", "withdrawn", "gone", "unsubscribed"];

const HOST_END: Record<HostEnd, TerminalEnd> = {
  expired: {
    reason: "expired",
    detail: "The terminal lease lapsed. Follow it again to continue.",
  },
  withdrawn: {
    reason: "denied",
    detail:
      "Your host no longer allows this view. Unlock it again to continue.",
  },
  gone: { reason: "gone", detail: "The terminal ended on your host." },
  unsubscribed: { reason: "closed", detail: "Closed." },
};

interface TerminalPiece {
  sub: number;
  seq: number;
  kind: "snapshot" | "update" | "end";
  cols: number;
  rows: number;
  data: Uint8Array;
  fin: boolean;
  end: HostEnd | null;
}

function terminalMessage(
  message: Record<string, unknown>,
): TerminalPiece | null {
  const count = (v: unknown) => Number.isSafeInteger(v) && (v as number) >= 0;
  const ending = message.kind === "end";
  if (
    Object.keys(message).sort().join() !==
      (ending
        ? "cols,data,end,fin,kind,rows,seq,sub,t"
        : "cols,data,fin,kind,rows,seq,sub,t") ||
    (ending && !HOST_ENDS.includes(message.end as HostEnd)) ||
    !count(message.sub) ||
    !count(message.seq) ||
    !["snapshot", "update", "end"].includes(message.kind as string) ||
    !count(message.cols) ||
    !count(message.rows) ||
    typeof message.data !== "string" ||
    typeof message.fin !== "boolean"
  ) {
    return null;
  }
  let data: Uint8Array;
  try {
    data = fromBase64Url(message.data);
  } catch {
    return null;
  }
  return {
    sub: message.sub as number,
    seq: message.seq as number,
    kind: message.kind as TerminalPiece["kind"],
    cols: message.cols as number,
    rows: message.rows as number,
    data,
    fin: message.fin,
    end: ending ? (message.end as HostEnd) : null,
  };
}

const broken = (detail: string) => ({
  end: { reason: "protocol" as const, detail },
  byHost: false,
});

/**
 * One followed terminal. Pieces must arrive exactly in order from seq 0, the
 * first being a snapshot; a frame is delivered once its last piece (`fin`) is in.
 */
class Following {
  private seq = 0;
  private parts: Uint8Array[] = [];
  private size = 0;
  private kind: "snapshot" | "update" | null = null;
  private started = false;
  readonly sub: number;
  private readonly onFrame: (frame: TerminalFrame) => void;
  readonly end: (end: TerminalEnd) => void;

  // Plain fields, not parameter properties: Node's strip-only TypeScript
  // (how the browser journey tests serve this file) cannot erase those.
  constructor(
    sub: number,
    onFrame: (frame: TerminalFrame) => void,
    end: (end: TerminalEnd) => void,
  ) {
    this.sub = sub;
    this.onFrame = onFrame;
    this.end = end;
  }

  /** `null`: keep following; otherwise why it stopped, and whether the host ended it. */
  take(piece: TerminalPiece): { end: TerminalEnd; byHost: boolean } | null {
    if (piece.seq !== this.seq)
      return broken("The terminal stream skipped or repeated a frame.");
    this.seq += 1;
    if (piece.kind === "end")
      return { end: HOST_END[piece.end ?? "gone"], byHost: true };
    if (!this.started && piece.kind !== "snapshot") {
      return broken("The terminal stream did not start with the screen.");
    }
    if (this.kind !== null && this.kind !== piece.kind) {
      return broken("The terminal stream interleaved frames.");
    }
    this.started = true;
    this.kind = piece.kind;
    this.size += piece.data.length;
    if (this.size > MAX_TERMINAL_FRAME)
      return broken("A terminal frame is too large.");
    this.parts.push(piece.data);
    if (!piece.fin) return null;
    const data = new Uint8Array(this.size);
    let at = 0;
    for (const part of this.parts) {
      data.set(part, at);
      at += part.length;
    }
    this.onFrame({
      kind: piece.kind,
      cols: piece.cols,
      rows: piece.rows,
      data,
    });
    this.parts = [];
    this.size = 0;
    this.kind = null;
    return null;
  }
}

function executed<K extends ReplyBody["kind"]>(
  result: ChannelResult,
  kind: K,
): Extract<ReplyBody, { kind: K }> {
  if (result.status !== "executed" || result.body?.kind !== kind) {
    throw new PrivateChannelError(
      result.detail ?? "Your host did not answer this read.",
      "refused",
    );
  }
  return result.body as Extract<ReplyBody, { kind: K }>;
}

/** One message of a private history, complete, in first-appearance order. */
export interface PrivateMessage {
  seq: number;
  uuid: string;
  sha256: string;
  message: Record<string, unknown>;
}

/**
 * Read one message in full: inline when it fits, otherwise in fragments whose
 * joined bytes must hash to the `sha256` the page named. Pieces of a message
 * that changed between reads never join (the host names the hash on every piece).
 */
async function complete(
  channel: PrivateChannel,
  session: string,
  item: HistoryItem,
  attempt = 0,
  signal?: AbortSignal,
): Promise<PrivateMessage> {
  signal?.throwIfAborted();
  if (item.message !== null) {
    return {
      seq: item.seq,
      uuid: item.uuid,
      sha256: item.sha256,
      message: item.message,
    };
  }
  if (item.size > MAX_ASSEMBLED_MESSAGE) {
    throw new PrivateChannelError(
      "A message is too large to show here.",
      "refused",
    );
  }
  const bytes = new Uint8Array(item.size);
  let offset: number | null = 0;
  while (offset !== null) {
    const piece: Extract<ReplyBody, { kind: "fragment" }> = executed(
      await channel.historyFragment(session, item.seq, offset),
      "fragment",
    );
    signal?.throwIfAborted();
    const data = fromBase64Url(piece.data);
    const end = offset + data.length;
    if (
      piece.seq !== item.seq ||
      piece.offset !== offset ||
      piece.size !== item.size ||
      piece.sha256 !== item.sha256 ||
      end > item.size ||
      (piece.next === null ? end !== item.size : piece.next !== end)
    ) {
      // The message changed while it was being read: start again from a fresh page.
      return reread(channel, session, item.seq, attempt + 1, signal);
    }
    bytes.set(data, offset);
    offset = piece.next;
  }
  if ((await sha256Bytes(bytes)) !== item.sha256) {
    throw new PrivateChannelError(
      "A message did not match its hash.",
      "untrusted",
    );
  }
  const message = JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  if (
    typeof message !== "object" ||
    message === null ||
    (message as Record<string, unknown>).uuid !== item.uuid
  ) {
    throw new PrivateChannelError(
      "A message did not match its entry.",
      "untrusted",
    );
  }
  return {
    seq: item.seq,
    uuid: item.uuid,
    sha256: item.sha256,
    message: message as Record<string, unknown>,
  };
}

const MAX_REREADS = 8;

/** Read the message at `seq` again from a fresh one-item page. */
async function reread(
  channel: PrivateChannel,
  session: string,
  seq: number,
  attempt = 0,
  signal?: AbortSignal,
): Promise<PrivateMessage> {
  signal?.throwIfAborted();
  if (attempt > MAX_REREADS) {
    throw new PrivateChannelError(
      "A message kept changing while it was read.",
      "unavailable",
    );
  }
  const page = executed(
    await channel.historyGet(session, seq - 1, 1),
    "history",
  );
  signal?.throwIfAborted();
  const item = page.items[0];
  if (!item || item.seq !== seq) {
    throw new PrivateChannelError(
      "A message disappeared while it was read.",
      "unavailable",
    );
  }
  return complete(channel, session, item, attempt, signal);
}

export interface HistoryReadOptions {
  /** Explicit resource limit: exceeding it fails visibly instead of truncating. */
  maxMessages?: number;
  signal?: AbortSignal;
  /** Complete, verified messages from this page only, in first-appearance order. */
  onPage?: (messages: PrivateMessage[]) => void;
}

/** Every message, with optional progressive delivery; never silently truncated. */
export async function readHistory(
  channel: PrivateChannel,
  session: string,
  options: HistoryReadOptions = {},
): Promise<PrivateMessage[]> {
  const out: PrivateMessage[] = [];
  let after: number | null = 0;
  while (after !== null) {
    options.signal?.throwIfAborted();
    const page: Extract<ReplyBody, { kind: "history" }> = executed(
      await channel.historyGet(session, after),
      "history",
    );
    options.signal?.throwIfAborted();
    if (page.next !== null && page.next <= after) {
      throw new PrivateChannelError(
        "Your host's history pagination did not advance.",
        "unavailable",
      );
    }
    if (out.length + page.items.length > (options.maxMessages ?? Infinity)) {
      throw new PrivateChannelError(
        "History exceeds the requested message limit; it was not read completely.",
        "unavailable",
      );
    }
    const completePage: PrivateMessage[] = [];
    for (const item of page.items) {
      options.signal?.throwIfAborted();
      completePage.push(
        await complete(channel, session, item, 0, options.signal),
      );
    }
    options.signal?.throwIfAborted();
    out.push(...completePage);
    options.onPage?.(completePage);
    after = page.next;
  }
  return out;
}

/**
 * Bring a read history up to date. A message keeps its `seq` when the agent
 * rewrites it (a tool result arriving, a streamed turn finishing), so reading
 * only `seq > last` would miss those changes: this compares every message's
 * hash from digest pages and re-reads the ones that changed or are new. The
 * result is a new array in first-appearance order.
 */
export async function refreshHistory(
  channel: PrivateChannel,
  session: string,
  known: PrivateMessage[],
  options: { signal?: AbortSignal } = {},
): Promise<{ messages: PrivateMessage[]; changed: number }> {
  const bySeq = new Map(known.map((m) => [m.seq, m]));
  const out: PrivateMessage[] = [];
  let changed = 0;
  let after: number | null = 0;
  while (after !== null) {
    options.signal?.throwIfAborted();
    const page: Extract<ReplyBody, { kind: "history" }> = executed(
      await channel.historyGet(session, after, 256, true),
      "history",
    );
    options.signal?.throwIfAborted();
    if (page.next !== null && page.next <= after) {
      throw new PrivateChannelError(
        "Your host's history pagination did not advance.",
        "unavailable",
      );
    }
    for (const item of page.items) {
      options.signal?.throwIfAborted();
      const have = bySeq.get(item.seq);
      if (have && have.sha256 === item.sha256 && have.uuid === item.uuid) {
        out.push(have);
      } else {
        out.push(await reread(channel, session, item.seq, 0, options.signal));
        changed += 1;
      }
    }
    after = page.next;
  }
  return { messages: out, changed };
}

/** Every session this key may read on the host, newest first. */
export async function listSessions(
  channel: PrivateChannel,
  options: { maxPages?: number; runningOnly?: boolean } = {},
): Promise<SessionEntry[]> {
  const out: SessionEntry[] = [];
  let after: string | null = null;
  const cursors = new Set<string>();
  for (let page = 0; page < (options.maxPages ?? Infinity); page++) {
    const body: Extract<ReplyBody, { kind: "sessions" }> = executed(
      await channel.sessionsList(after, undefined, options.runningOnly),
      "sessions",
    );
    out.push(...body.sessions);
    if (body.next === null) break;
    if (cursors.has(body.next)) {
      throw new PrivateChannelError(
        "Your host's session pagination did not advance. Retry the session list.",
        "unavailable",
      );
    }
    cursors.add(body.next);
    after = body.next;
  }
  return out;
}

/** The host's launch settings, read over the channel. */
export async function readSettings(
  channel: PrivateChannel,
): Promise<HostLaunchSettings> {
  return executed(await channel.settingsGet(), "settings").launch;
}

async function selectedPair(
  pc: Pick<RTCPeerConnection, "getStats">,
): Promise<SelectedPair | null> {
  const stats = await pc.getStats();
  const byId = new Map<string, Record<string, unknown>>();
  stats.forEach((s: Record<string, unknown>) => byId.set(s.id as string, s));
  let out: SelectedPair | null = null;
  stats.forEach((s: Record<string, unknown>) => {
    if (s.type !== "transport" || typeof s.selectedCandidatePairId !== "string")
      return;
    const pair = byId.get(s.selectedCandidatePairId);
    const local = byId.get(pair?.localCandidateId as string);
    const remote = byId.get(pair?.remoteCandidateId as string);
    out = {
      localType: (local?.candidateType as string) ?? null,
      remoteType: (remote?.candidateType as string) ?? null,
      relayProtocol: (local?.relayProtocol as string) ?? null,
    };
  });
  return out;
}
