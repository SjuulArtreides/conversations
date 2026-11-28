/**
 * Browser-held signing identity for host control.
 *
 * A non-extractable ECDSA P-256 key lives in IndexedDB (the private key can be
 * used but never read, so a script that gets the page cannot copy it away).
 * A passkey approval makes the HOST accept that key for 24 hours (a
 * "delegation"); operations are then signed with it and verified by the host.
 * Host authorization stays local. Publishing this key in the server directory
 * proves possession but grants no host access.
 *
 * The signature covers the exact payload bytes behind a domain prefix; the
 * host verifies those same bytes, so nothing is re-serialised in between.
 */

const DB_NAME = "conversations-host-auth";
const KEY_STORE = "keys";
const DELEGATION_STORE = "delegations";
const BROWSER_KEY_ID = "browser";
const DOMAIN = "conversations-op/v1\n";
/** Operations are valid for this long after signing. */
export const OP_LIFETIME_SECS = 60;
/** Input-class frames (keys, resize, repaint) live seconds: the host does not journal them. */
export const STREAM_LIFETIME_SECS = 20;
/** A delegation closer than this to expiry is treated as expired. */
export const DELEGATION_MARGIN_SECS = 120;

export interface BrowserKey {
  privateKey: CryptoKey;
  /** Uncompressed P-256 point (65 bytes). */
  publicRaw: Uint8Array<ArrayBuffer>;
  /** base64url(SHA-256(publicRaw)): the id the host knows this key by. */
  kid: string;
}

export interface Delegation {
  hostId: string;
  dlg: string;
  kid: string;
  /** Unix seconds, as stated by the host. */
  notAfter: number;
  /** What the host actually delegated (the request narrowed to the passkey's ceiling). */
  caps?: string[];
  /** Host credential id of the passkey that approved this delegation. */
  parentKid?: string;
}

export interface SignedFrame {
  p: string;
  s: string;
  k: string;
}

export function toBase64Url(bytes: Uint8Array): string {
  return toBase64(bytes)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

export function fromBase64Url(text: string): Uint8Array<ArrayBuffer> {
  const padded = text.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}

export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function randomHex(bytes: number): string {
  const buffer = new Uint8Array(bytes);
  crypto.getRandomValues(buffer);
  return Array.from(buffer, (b) => b.toString(16).padStart(2, "0")).join("");
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(KEY_STORE);
      request.result.createObjectStore(DELEGATION_STORE);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

/**
 * Run one transaction and resolve only once it has COMMITTED. An abort (quota,
 * eviction, a concurrent version change) rejects, so a caller never reports
 * something as saved that was not.
 */
async function inTransaction<T>(
  store: string,
  mode: IDBTransactionMode,
  run: (store: IDBObjectStore, setResult: (value: T) => void) => void,
): Promise<T> {
  const db = await openDb();
  try {
    return await new Promise<T>((resolve, reject) => {
      let result: T;
      const tx = db.transaction(store, mode);
      tx.oncomplete = () => resolve(result);
      tx.onabort = () =>
        reject(tx.error ?? new Error("The browser did not save this."));
      tx.onerror = () => reject(tx.error);
      run(tx.objectStore(store), (value) => {
        result = value;
      });
    });
  } finally {
    db.close();
  }
}

function withStore<T>(
  store: string,
  mode: IDBTransactionMode,
  run: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  return inTransaction<T>(store, mode, (objectStore, setResult) => {
    const request = run(objectStore);
    request.onsuccess = () => setResult(request.result);
  });
}

async function describeKey(pair: CryptoKeyPair): Promise<BrowserKey> {
  const publicRaw = new Uint8Array(
    await crypto.subtle.exportKey("raw", pair.publicKey),
  );
  const digest = new Uint8Array(
    await crypto.subtle.digest("SHA-256", publicRaw),
  );
  return { privateKey: pair.privateKey, publicRaw, kid: toBase64Url(digest) };
}

/**
 * The persisted browser key, created (non-extractable) on first use. Several
 * tabs may call this at once: the candidate is generated first, then ONE
 * read-write transaction keeps whichever key is already stored or stores the
 * candidate, so every caller converges on the same persisted key.
 */
export async function loadBrowserKey(): Promise<BrowserKey> {
  const stored = await withStore<CryptoKeyPair | undefined>(
    KEY_STORE,
    "readonly",
    (s) => s.get(BROWSER_KEY_ID),
  );
  if (stored) return describeKey(stored);
  const candidate = (await crypto.subtle.generateKey(
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["sign", "verify"],
  )) as CryptoKeyPair;
  const winner = await inTransaction<CryptoKeyPair>(
    KEY_STORE,
    "readwrite",
    (s, setResult) => {
      const read = s.get(BROWSER_KEY_ID);
      read.onsuccess = () => {
        if (read.result) {
          setResult(read.result as CryptoKeyPair);
        } else {
          s.put(candidate, BROWSER_KEY_ID);
          setResult(candidate);
        }
      };
    },
  );
  return describeKey(winner);
}

/**
 * The stored delegation for a host, if it is still usable AND belongs to the
 * browser key that exists now (a delegation for another key is discarded).
 */
export async function getDelegation(
  hostId: string,
  now: number = Math.floor(Date.now() / 1000),
): Promise<Delegation | null> {
  const stored = await withStore<Delegation | undefined>(
    DELEGATION_STORE,
    "readonly",
    (s) => s.get(hostId),
  );
  if (!stored) return null;
  const key = await loadBrowserKey();
  if (stored.kid !== key.kid) {
    await forgetDelegation(hostId);
    return null;
  }
  if (stored.notAfter - now < DELEGATION_MARGIN_SECS) return null;
  return stored;
}

export async function saveDelegation(delegation: Delegation): Promise<void> {
  await withStore(DELEGATION_STORE, "readwrite", (s) =>
    s.put(delegation, delegation.hostId),
  );
}

export async function forgetDelegation(hostId: string): Promise<void> {
  await withStore(DELEGATION_STORE, "readwrite", (s) => s.delete(hostId));
}

export type OperationName =
  | "migration.export"
  | "migration.import"
  | "session.start"
  | "session.reopen"
  | "session.rename"
  | "communications.list"
  | "attention.recipients"
  | "attention.request"
  | "notifications.list"
  | "notifications.read"
  | "notifications.resolve"
  | "linear.list"
  | "linear.unlink"
  | "access.list"
  | "projects.list"
  | "projects.create"
  | "session.project.get"
  | "session.project.set"
  | "session.input"
  | "session.reply"
  | "session.resize"
  | "session.repaint"
  | "session.end"
  | "session.paste_image"
  | "session.send_files"
  | "pr.cache.list"
  | "pr.unlink"
  | "pr.sync"
  | "pr.comment"
  | "settings.get"
  | "settings.update"
  | "grant.add"
  | "grant.remove"
  | "publication.share"
  | "publication.unshare"
  | "publication.status"
  | "passkey.enroll_start"
  | "passkey.pending"
  | "passkey.approve"
  | "browser_trust.start"
  | "browser_trust.finish"
  | "passkey.vouch"
  | "machine.pair"
  | "machine.unpair"
  | "peer.offer"
  | "history.get"
  | "history.fragment"
  | "history.search"
  | "sessions.list"
  | "terminal.subscribe";

/**
 * `sess` of an operation about the host itself (settings), not a session. Such an
 * operation carries no session argument at all, so it can never be confused with
 * one aimed at a session.
 */
export const HOST_SCOPE = "host";

export interface OperationArgs {
  hostId: string;
  key: BrowserKey;
  /** The delegation the key acts under; `null` for a key the host approved directly. */
  dlg: string | null;
  op: OperationName;
  /** The session the operation is about (for grant.add: the session being shared). */
  session: string;
  /** Extra arguments of the operation. `session` is always included. */
  args?: Record<string, unknown>;
  /** Seconds the host will accept it; defaults to {@link OP_LIFETIME_SECS}. */
  lifetime?: number;
  now?: number;
}

/**
 * The exact payload bytes the host will verify. Key order is irrelevant to the
 * host (it parses strictly) but fixed here so the bytes are reproducible.
 */
export function operationPayload(input: OperationArgs): string {
  const iat = input.now ?? Math.floor(Date.now() / 1000);
  return JSON.stringify({
    v: 1,
    typ: "op",
    aud: input.hostId,
    kid: input.key.kid,
    dlg: input.dlg,
    op: input.op,
    sess: input.session,
    jti: randomHex(16),
    iat,
    exp: iat + (input.lifetime ?? OP_LIFETIME_SECS),
    args:
      input.session === HOST_SCOPE
        ? { ...(input.args ?? {}) }
        : { session: input.session, ...(input.args ?? {}) },
  });
}

/** Sign payload bytes: ECDSA P-256 / SHA-256 over DOMAIN || payload, raw r||s. */
export async function signPayload(
  key: BrowserKey,
  payload: string,
): Promise<SignedFrame> {
  const bytes = new TextEncoder().encode(payload);
  const signed = new Uint8Array(DOMAIN.length + bytes.length);
  signed.set(new TextEncoder().encode(DOMAIN));
  signed.set(bytes, DOMAIN.length);
  const signature = new Uint8Array(
    await crypto.subtle.sign(
      { name: "ECDSA", hash: "SHA-256" },
      key.privateKey,
      signed,
    ),
  );
  return {
    p: toBase64Url(bytes),
    s: toBase64Url(signature),
    k: key.kid,
  };
}

/** Publish only this browser's key, for the signed-in account and chosen label.
 * Registration uses its own domain and is never an executable host operation.
 */
export async function signBrowserKeyRegistration(
  key: BrowserKey,
  challenge: { challenge: string; payload: string },
  email: string,
  label: string,
): Promise<string> {
  const value = JSON.parse(challenge.payload) as Record<string, unknown>;
  const now = Math.floor(Date.now() / 1000);
  if (
    value.v !== 1 ||
    value.purpose !== "publish_browser_key" ||
    value.user_email !== email ||
    value.public_key !== toBase64(key.publicRaw) ||
    value.kid !== key.kid ||
    value.label !== label.trim() ||
    value.challenge !== challenge.challenge ||
    !/^[a-f0-9]{32}$/.test(challenge.challenge) ||
    typeof value.expires_at !== "number" ||
    value.expires_at < now ||
    value.expires_at > now + 330
  )
    throw new Error(
      "The publication challenge does not match this browser and account.",
    );
  const signature = await crypto.subtle.sign(
    { name: "ECDSA", hash: "SHA-256" },
    key.privateKey,
    new TextEncoder().encode(
      "conversations-browser-key/v1\n" + challenge.payload,
    ),
  );
  return toBase64Url(new Uint8Array(signature));
}
