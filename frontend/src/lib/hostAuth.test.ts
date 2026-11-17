import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  forgetDelegation,
  fromBase64Url,
  getDelegation,
  loadBrowserKey,
  operationPayload,
  saveDelegation,
  signPayload,
  toBase64Url,
} from "./hostAuth";

const HOST = "A".repeat(43);

beforeEach(async () => {
  indexedDB.deleteDatabase("conversations-host-auth");
});

describe("browser signing key", () => {
  it("is non-extractable and reused across loads", async () => {
    const first = await loadBrowserKey();
    expect(first.privateKey.extractable).toBe(false);
    await expect(
      crypto.subtle.exportKey("pkcs8", first.privateKey),
    ).rejects.toThrow();
    const second = await loadBrowserKey();
    expect(second.kid).toBe(first.kid);
    expect(Array.from(second.publicRaw)).toEqual(Array.from(first.publicRaw));
    expect(first.publicRaw).toHaveLength(65);
  });

  it("signs the domain-prefixed exact payload bytes, verifiable with the public key", async () => {
    const key = await loadBrowserKey();
    const payload = operationPayload({
      hostId: HOST,
      key,
      dlg: "0".repeat(32),
      op: "session.start",
      session: "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
      args: { agent: "claude" },
      now: 1_800_000_000,
    });
    const frame = await signPayload(key, payload);
    expect(frame.k).toBe(key.kid);
    // The relayed bytes are exactly what was signed.
    expect(new TextDecoder().decode(fromBase64Url(frame.p))).toBe(payload);
    const publicKey = await crypto.subtle.importKey(
      "raw",
      key.publicRaw,
      { name: "ECDSA", namedCurve: "P-256" },
      true,
      ["verify"],
    );
    const signed = new TextEncoder().encode("conversations-op/v1\n" + payload);
    const verify = (bytes: Uint8Array<ArrayBuffer>) =>
      crypto.subtle.verify(
        { name: "ECDSA", hash: "SHA-256" },
        publicKey,
        fromBase64Url(frame.s),
        bytes,
      );
    expect(await verify(signed)).toBe(true);
    // Any change to the payload, or a missing domain prefix, fails.
    const tampered = signed.slice();
    tampered[tampered.length - 5] ^= 1;
    expect(await verify(tampered)).toBe(false);
    expect(await verify(new TextEncoder().encode(payload))).toBe(false);
  });

  it("builds a bounded, strictly-shaped operation", async () => {
    const key = await loadBrowserKey();
    const payload = JSON.parse(
      operationPayload({
        hostId: HOST,
        key,
        dlg: "1".repeat(32),
        op: "session.reopen",
        session: "5e1aeb93-5aba-419e-8856-a7c721406279",
        now: 1000,
      }),
    );
    expect(Object.keys(payload).sort()).toEqual(
      [
        "args",
        "aud",
        "dlg",
        "exp",
        "iat",
        "jti",
        "kid",
        "op",
        "sess",
        "typ",
        "v",
      ].sort(),
    );
    expect(payload.exp - payload.iat).toBe(60);
    expect(payload.aud).toBe(HOST);
    expect(payload.args).toEqual({ session: payload.sess });
    expect(payload.jti).toMatch(/^[0-9a-f]{32}$/);
    // Nothing about folders, prompts or resume ids can ride along.
    expect(JSON.stringify(payload)).not.toMatch(/cwd|resume|prompt/);
  });
});

describe("concurrent key creation", () => {
  it("converges every tab on one persisted key", async () => {
    const keys = await Promise.all(
      Array.from({ length: 8 }, () => loadBrowserKey()),
    );
    expect(new Set(keys.map((k) => k.kid)).size).toBe(1);
    expect((await loadBrowserKey()).kid).toBe(keys[0].kid);
  });
});

describe("delegation window", () => {
  const base = { hostId: HOST, dlg: "2".repeat(32) };
  let kid: string;
  beforeEach(async () => {
    kid = (await loadBrowserKey()).kid;
  });

  it("is usable until shortly before expiry, then requires a new passkey approval", async () => {
    await saveDelegation({ ...base, kid, notAfter: 100_000 });
    expect(await getDelegation(HOST, 100_000 - 3600)).not.toBeNull();
    // Inside the safety margin an operation could outlive the delegation.
    expect(await getDelegation(HOST, 100_000 - 60)).toBeNull();
    expect(await getDelegation(HOST, 100_001)).toBeNull();
  });

  it("is per host and can be locked", async () => {
    await saveDelegation({ ...base, kid, notAfter: 100_000 });
    expect(await getDelegation("B".repeat(43), 0)).toBeNull();
    await forgetDelegation(HOST);
    expect(await getDelegation(HOST, 0)).toBeNull();
  });

  it("discards a delegation that belongs to a different key", async () => {
    await saveDelegation({ ...base, kid: "someone-elses", notAfter: 100_000 });
    expect(await getDelegation(HOST, 0)).toBeNull();
    // It was removed, not just ignored.
    await saveDelegation({ ...base, kid, notAfter: 100_000 });
    expect(await getDelegation(HOST, 0)).not.toBeNull();
  });

  it("does not report a delegation as saved when the transaction aborts", async () => {
    const put = vi
      .spyOn(IDBObjectStore.prototype, "put")
      .mockImplementation(function (this: IDBObjectStore) {
        this.transaction.abort();
        return {} as IDBRequest;
      });
    await expect(
      saveDelegation({ ...base, kid, notAfter: 100_000 }),
    ).rejects.toBeDefined();
    put.mockRestore();
    expect(await getDelegation(HOST, 0)).toBeNull();
  });
});

describe("base64url", () => {
  it("round-trips arbitrary bytes", () => {
    const bytes = Uint8Array.from([0, 250, 251, 252, 253, 254, 255, 62, 63]);
    expect(Array.from(fromBase64Url(toBase64Url(bytes)))).toEqual(
      Array.from(bytes),
    );
  });
});

it("publishes only an account/key/purpose-bound proof under its own domain", async () => {
  const { signBrowserKeyRegistration, toBase64 } = await import("./hostAuth");
  const key = await loadBrowserKey();
  const statement = {
    v: 1,
    purpose: "publish_browser_key",
    user_email: "alice@example.com",
    public_key: toBase64(key.publicRaw),
    kid: key.kid,
    label: "Laptop",
    challenge: "a".repeat(32),
    expires_at: Math.floor(Date.now() / 1000) + 300,
  };
  const input = {
    challenge: statement.challenge,
    payload: JSON.stringify(statement),
  };
  const signature = await signBrowserKeyRegistration(
    key,
    input,
    "alice@example.com",
    "Laptop",
  );
  const publicKey = await crypto.subtle.importKey(
    "raw",
    key.publicRaw,
    { name: "ECDSA", namedCurve: "P-256" },
    true,
    ["verify"],
  );
  expect(
    await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      publicKey,
      fromBase64Url(signature),
      new TextEncoder().encode(
        "conversations-browser-key/v1\n" + input.payload,
      ),
    ),
  ).toBe(true);
  for (const change of [
    { user_email: "bob@example.com" },
    { kid: "B".repeat(43) },
    { public_key: toBase64(new Uint8Array(65)) },
    { purpose: "other" },
    { challenge: "b".repeat(32) },
    { expires_at: 0 },
    { label: "Someone else's browser" },
  ]) {
    await expect(
      signBrowserKeyRegistration(
        key,
        { ...input, payload: JSON.stringify({ ...statement, ...change }) },
        "alice@example.com",
        "Laptop",
      ),
    ).rejects.toThrow();
  }
});
