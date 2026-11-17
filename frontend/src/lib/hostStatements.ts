/**
 * Verification of what the host says, done in the browser.
 *
 * The relay between this page and the host is untrusted: it can drop, replay or
 * rewrite anything. So nothing it returns is believed unless it is a statement
 * signed by the host key pinned at pairing (Ed25519 over `HOST_DOMAIN ||
 * payload`), and every field of that statement is compared with what this page
 * asked for. Delegation offers are checked BEFORE a passkey prompt is shown,
 * so a request altered in transit is rejected without the user approving it.
 */

import { fromBase64Url, toBase64Url } from "./hostAuth";
import {
  CODEX_APPROVAL_POLICIES,
  CODEX_SANDBOX_MODES,
  type AgentLauncher,
  type CodexApprovalPolicy,
  type CodexSandboxMode,
} from "./types";

const HOST_DOMAIN = "conversations-host/v1\n";
export const DESCRIPTOR_PREFIX = "chd1.";
/** Payloads are tiny; anything bigger is not from an honest host. */
const MAX_PAYLOAD_BYTES = 4096;
const MAX_DETAIL_CHARS = 160;
/** An offer older or newer than this (seconds) against our clock is rejected. */
export const OFFER_CLOCK_SKEW_SECS = 300;
/** The host's fixed delegation lifetime. */
export const DELEGATION_SECS = 86_400;

export interface PinnedHost {
  /** base64url(SHA-256(key)). */
  id: string;
  /** base64url(Ed25519 public key, 32 bytes). */
  key: string;
}

export class StatementError extends Error {}

type Payload = Record<string, unknown>;

async function sha256(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  return toBase64Url(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
  );
}

/**
 * Parse a descriptor printed on the host (`chd1.<key>`), derive the id from the
 * key and return both. The id is never taken from anywhere else.
 */
export async function parseDescriptor(text: string): Promise<PinnedHost> {
  const trimmed = text.trim();
  if (!trimmed.startsWith(DESCRIPTOR_PREFIX)) {
    throw new StatementError(
      "That is not a host descriptor. Run `arrakis-conversations host credential descriptor` on the host.",
    );
  }
  const key = trimmed.slice(DESCRIPTOR_PREFIX.length);
  if (!/^[A-Za-z0-9_-]{43}$/.test(key)) {
    throw new StatementError("The host descriptor is malformed.");
  }
  const bytes = fromBase64Url(key);
  if (bytes.length !== 32 || toBase64Url(bytes) !== key) {
    throw new StatementError("The host descriptor is malformed.");
  }
  return { id: await sha256(bytes), key };
}

/** True when `id` really is the hash of `key` (a stored pin was not edited). */
export async function pinIsConsistent(host: PinnedHost): Promise<boolean> {
  try {
    return (await parseDescriptor(DESCRIPTOR_PREFIX + host.key)).id === host.id;
  } catch {
    return false;
  }
}

/** Verify a host statement and return its payload, or throw. */
export async function verifyStatement(
  host: PinnedHost,
  statement: unknown,
  maxPayloadBytes: number = MAX_PAYLOAD_BYTES,
): Promise<{ payload: Payload }> {
  const reject = (why: string): never => {
    throw new StatementError(
      `The answer is not authentic (${why}); it did not come from your host.`,
    );
  };
  if (
    typeof statement !== "object" ||
    statement === null ||
    Object.keys(statement).sort().join() !== "p,s" ||
    typeof (statement as Payload).p !== "string" ||
    typeof (statement as Payload).s !== "string"
  ) {
    return reject("malformed");
  }
  const { p, s } = statement as { p: string; s: string };
  if (p.length > maxPayloadBytes * 2) return reject("too large");
  let payloadBytes: Uint8Array<ArrayBuffer>;
  let signature: Uint8Array<ArrayBuffer>;
  try {
    payloadBytes = fromBase64Url(p);
    signature = fromBase64Url(s);
  } catch {
    return reject("malformed");
  }
  if (payloadBytes.length > maxPayloadBytes) return reject("too large");
  if (signature.length !== 64) return reject("bad signature");
  const message = new Uint8Array(HOST_DOMAIN.length + payloadBytes.length);
  message.set(new TextEncoder().encode(HOST_DOMAIN));
  message.set(payloadBytes, HOST_DOMAIN.length);
  let ok: boolean;
  try {
    const key = await crypto.subtle.importKey(
      "raw",
      fromBase64Url(host.key),
      "Ed25519",
      false,
      ["verify"],
    );
    ok = await crypto.subtle.verify("Ed25519", key, signature, message);
  } catch {
    ok = false;
  }
  if (!ok) return reject("signature does not match the pinned host key");
  let payload: unknown;
  try {
    payload = JSON.parse(new TextDecoder().decode(payloadBytes));
  } catch {
    return reject("malformed");
  }
  if (typeof payload !== "object" || payload === null || Array.isArray(payload))
    return reject("malformed");
  const fields = payload as Payload;
  if (fields.v !== 1) return reject("unknown version");
  if (fields.host !== host.id) return reject("signed for another host");
  return { payload: fields };
}

export function exactKeys(payload: Payload, keys: string[]) {
  if (Object.keys(payload).sort().join() !== [...keys].sort().join()) {
    throw new StatementError(
      "The host's answer has unexpected fields and was rejected.",
    );
  }
}

function mismatch(field: string): never {
  throw new StatementError(
    `The host's answer does not match what was requested (${field}); it was rejected before any passkey prompt. Something between this page and your host altered the request.`,
  );
}

/** Everything the page chose when it asked for a delegation. */
export interface DelegationRequest {
  /** Standard base64 of the 65-byte uncompressed P-256 point. */
  browserPub: string;
  kid: string;
  dlg: string;
  /** Sorted, duplicate-free. */
  caps: string[];
  nonce: string;
}

export interface OfferReply {
  challenge: string;
  options: unknown;
  statement: unknown;
}

export interface VerifiedOffer {
  notAfter: number;
  issuedAt: number;
}

/**
 * Compare the host-signed offer with the request this page made and with the
 * WebAuthn options the relay handed over. Throws unless they all agree.
 */
export async function checkOffer(
  host: PinnedHost,
  request: DelegationRequest,
  reply: OfferReply,
  now: number = Math.floor(Date.now() / 1000),
): Promise<VerifiedOffer> {
  const { payload } = await verifyStatement(host, reply.statement);
  exactKeys(payload, [
    "v",
    "typ",
    "host",
    "nonce",
    "ceremony",
    "browser_kid",
    "browser_pub",
    "dlg",
    "caps",
    "issued_at",
    "not_after",
    "webauthn_challenge",
    "rp_id",
  ]);
  if (payload.typ !== "delegate_offer") mismatch("type");
  if (payload.nonce !== request.nonce) mismatch("nonce");
  if (payload.browser_pub !== request.browserPub) mismatch("browser key");
  if (payload.browser_kid !== request.kid) mismatch("browser key id");
  if (payload.dlg !== request.dlg) mismatch("delegation id");
  if (!sameList(payload.caps, request.caps)) mismatch("scope");
  if (payload.ceremony !== reply.challenge) mismatch("ceremony");
  const publicKey = (reply.options as { publicKey?: Payload } | null)
    ?.publicKey;
  if (
    typeof payload.webauthn_challenge !== "string" ||
    payload.webauthn_challenge !== publicKey?.challenge
  ) {
    mismatch("passkey challenge");
  }
  if (typeof payload.rp_id !== "string" || payload.rp_id !== publicKey?.rpId) {
    mismatch("relying party");
  }
  const { issued_at: issuedAt, not_after: notAfter } = payload;
  if (!Number.isSafeInteger(issuedAt) || !Number.isSafeInteger(notAfter)) {
    mismatch("expiry");
  }
  if (
    (notAfter as number) - (issuedAt as number) !== DELEGATION_SECS ||
    Math.abs((issuedAt as number) - now) > OFFER_CLOCK_SKEW_SECS
  ) {
    mismatch("expiry");
  }
  return { issuedAt: issuedAt as number, notAfter: notAfter as number };
}

/** The host's completion statement must close exactly the offer we accepted. */
export async function checkDone(
  host: PinnedHost,
  request: DelegationRequest,
  ceremony: string,
  offer: VerifiedOffer,
  statement: unknown,
): Promise<{ notAfter: number; caps: string[] }> {
  const { payload } = await verifyStatement(host, statement);
  exactKeys(payload, [
    "v",
    "typ",
    "host",
    "nonce",
    "ceremony",
    "browser_kid",
    "dlg",
    "caps",
    "not_after",
  ]);
  if (payload.typ !== "delegate_done") mismatch("type");
  if (payload.nonce !== request.nonce) mismatch("nonce");
  if (payload.ceremony !== ceremony) mismatch("ceremony");
  if (payload.browser_kid !== request.kid) mismatch("browser key id");
  if (payload.dlg !== request.dlg) mismatch("delegation id");
  // The host may NARROW the scope to what the passkey is approved for; it can
  // never widen it. The delegated scope is what it states here.
  const caps = payload.caps;
  if (
    !Array.isArray(caps) ||
    caps.length === 0 ||
    caps.some(
      (cap) => typeof cap !== "string" || !request.caps.includes(cap),
    ) ||
    new Set(caps).size !== caps.length
  ) {
    mismatch("scope");
  }
  if (payload.not_after !== offer.notAfter) mismatch("expiry");
  return { notAfter: offer.notAfter, caps: caps as string[] };
}

const B64URL_ID = /^[A-Za-z0-9_-]{43}$/;

/** Raw bytes of what an authenticator returned at registration. */
export interface RegistrationParts {
  rawId: Uint8Array<ArrayBuffer>;
  attestationObject: Uint8Array<ArrayBuffer>;
  clientDataJSON: Uint8Array<ArrayBuffer>;
}

/**
 * Domain-separated, length-delimited digest of a registration response; the
 * host computes the same value over the response it verified and stored.
 */
export async function registrationDigest(
  parts: RegistrationParts,
): Promise<string> {
  const chunks = [parts.rawId, parts.attestationObject, parts.clientDataJSON];
  const prefix = new TextEncoder().encode("conversations-reg/v1\n");
  const total = chunks.reduce((n, c) => n + 4 + c.length, prefix.length);
  const message = new Uint8Array(total);
  message.set(prefix);
  let at = prefix.length;
  for (const chunk of chunks) {
    new DataView(message.buffer).setUint32(at, chunk.length, false);
    message.set(chunk, at + 4);
    at += 4 + chunk.length;
  }
  return sha256(message);
}

/** The id the host gives a passkey: base64url(SHA-256(raw credential id)). */
export function expectedPasskeyId(
  rawId: Uint8Array<ArrayBuffer>,
): Promise<string> {
  return sha256(rawId);
}

/**
 * The host's completion of a registration. It must be bound to this ceremony,
 * our nonce and the digest of the exact response our authenticator produced.
 * Returns the passkey id the HOST says it stored, already proven equal to the
 * one derived locally.
 */
export async function checkEnrolled(
  host: PinnedHost,
  expected: {
    ceremony: string;
    nonce: string;
    credId: string;
    responseDigest: string;
  },
  statement: unknown,
): Promise<string> {
  const { payload } = await verifyStatement(host, statement);
  exactKeys(payload, [
    "v",
    "typ",
    "host",
    "nonce",
    "ceremony",
    "cred_id",
    "response_sha256",
  ]);
  if (payload.typ !== "enroll_done") mismatch("type");
  if (payload.nonce !== expected.nonce) mismatch("nonce");
  if (payload.ceremony !== expected.ceremony) mismatch("ceremony");
  if (
    typeof payload.cred_id !== "string" ||
    !B64URL_ID.test(payload.cred_id) ||
    payload.cred_id !== expected.credId
  ) {
    mismatch("passkey id");
  }
  if (
    typeof payload.response_sha256 !== "string" ||
    !B64URL_ID.test(payload.response_sha256) ||
    payload.response_sha256 !== expected.responseDigest
  ) {
    mismatch("registration");
  }
  return expected.credId;
}

/** The launch flags a host keeps locally and signs for `settings.get`. */
export interface HostLaunchSettings {
  claude_skip_permissions: boolean;
  opencode_auto: boolean;
  codex_sandbox_mode: CodexSandboxMode | null;
  codex_approval_policy: CodexApprovalPolicy | null;
  default_launcher: AgentLauncher;
  adk_statusline: boolean;
}

/**
 * The launch block of the host's `settings.get` reply (which arrives only on the
 * private channel, inside a host-signed reply bound to the exact request). It is
 * closed — exactly these six flags, each of the right type. Anything else (a
 * path, an origin, an extra key) is rejected.
 */
export function checkLaunch(launch: unknown): HostLaunchSettings {
  if (typeof launch !== "object" || launch === null || Array.isArray(launch)) {
    mismatch("settings");
  }
  const l = launch as Payload;
  exactKeys(l, [
    "claude_skip_permissions",
    "opencode_auto",
    "codex_sandbox_mode",
    "codex_approval_policy",
    "default_launcher",
    "adk_statusline",
  ]);
  const flag = (value: unknown, name: string): boolean => {
    if (typeof value !== "boolean") mismatch(name);
    return value as boolean;
  };
  const choice = <T extends string>(
    value: unknown,
    allowed: readonly T[],
    name: string,
  ): T | null => {
    if (value === null) return null;
    if (typeof value !== "string" || !allowed.includes(value as T)) {
      mismatch(name);
    }
    return value as T;
  };
  if (l.default_launcher !== "opencode" && l.default_launcher !== "adk") {
    mismatch("launcher");
  }
  return {
    claude_skip_permissions: flag(l.claude_skip_permissions, "claude flag"),
    opencode_auto: flag(l.opencode_auto, "opencode flag"),
    codex_sandbox_mode: choice(
      l.codex_sandbox_mode,
      CODEX_SANDBOX_MODES,
      "codex sandbox",
    ),
    codex_approval_policy: choice(
      l.codex_approval_policy,
      CODEX_APPROVAL_POLICIES,
      "codex approvals",
    ),
    default_launcher: l.default_launcher as AgentLauncher,
    adk_statusline: flag(l.adk_statusline, "adk flag"),
  };
}

export const OP_STATUSES = [
  "executed",
  "replayed",
  "refused",
  "failed",
  "uncertain",
] as const;
export type OpStatus = (typeof OP_STATUSES)[number];

/**
 * The host's outcome for one signed operation. It is bound to the SHA-256 of
 * the exact operation bytes this page sent and to the signing key id, so it
 * cannot be a forged success or the answer to a different operation.
 */
export async function checkOpResult(
  host: PinnedHost,
  sent: { kid: string; payloadB64u: string },
  statement: unknown,
): Promise<{ status: OpStatus; detail: string | null }> {
  const { payload } = await verifyStatement(host, statement);
  exactKeys(payload, [
    "v",
    "typ",
    "host",
    "kid",
    "payload_sha256",
    "status",
    "detail",
  ]);
  if (payload.typ !== "op_result") mismatch("type");
  if (payload.kid !== sent.kid) mismatch("key id");
  if (
    payload.payload_sha256 !== (await sha256(fromBase64Url(sent.payloadB64u)))
  )
    mismatch("operation");
  if (!OP_STATUSES.includes(payload.status as OpStatus)) mismatch("status");
  const detail = payload.detail;
  if (
    detail !== null &&
    (typeof detail !== "string" || detail.length > MAX_DETAIL_CHARS)
  ) {
    mismatch("detail");
  }
  return {
    status: payload.status as OpStatus,
    detail: detail as string | null,
  };
}

function sameList(actual: unknown, expected: string[]): boolean {
  return (
    Array.isArray(actual) &&
    actual.length === expected.length &&
    actual.every((value, index) => value === expected[index])
  );
}
