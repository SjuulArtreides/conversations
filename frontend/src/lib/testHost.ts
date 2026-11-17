/**
 * A software stand-in for the host's statement issuer, for tests only: an
 * Ed25519 key that signs statements in the same byte format as the real host
 * (`authz/attest.rs`). The real issuer is covered by the committed fixture
 * `fixtures/host_statements.json`, which the Rust host generates.
 */

import { fromBase64Url, toBase64Url } from "./hostAuth";
import { DESCRIPTOR_PREFIX, type PinnedHost } from "./hostStatements";

export interface TestHost extends PinnedHost {
  descriptor: string;
  sign(payload: unknown): Promise<{ p: string; s: string }>;
}

export async function makeTestHost(): Promise<TestHost> {
  const pair = (await crypto.subtle.generateKey("Ed25519", true, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  const raw = new Uint8Array(
    await crypto.subtle.exportKey("raw", pair.publicKey),
  );
  const key = toBase64Url(raw);
  const id = toBase64Url(
    new Uint8Array(await crypto.subtle.digest("SHA-256", raw)),
  );
  return {
    id,
    key,
    descriptor: DESCRIPTOR_PREFIX + key,
    async sign(payload) {
      const bytes = new TextEncoder().encode(
        typeof payload === "string" ? payload : JSON.stringify(payload),
      );
      const message = new Uint8Array(
        "conversations-host/v1\n".length + bytes.length,
      );
      message.set(new TextEncoder().encode("conversations-host/v1\n"));
      message.set(bytes, "conversations-host/v1\n".length);
      const signature = new Uint8Array(
        await crypto.subtle.sign("Ed25519", pair.privateKey, message),
      );
      return { p: toBase64Url(bytes), s: toBase64Url(signature) };
    },
  };
}

export async function sha256b64u(payloadB64u: string): Promise<string> {
  return toBase64Url(
    new Uint8Array(
      await crypto.subtle.digest("SHA-256", fromBase64Url(payloadB64u)),
    ),
  );
}
