import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  fromBase64Url,
  getDelegation,
  loadBrowserKey,
  saveDelegation,
  toBase64Url,
} from "./hostAuth";
import {
  clampUtf8,
  closePrivateChannels,
  createInputChannel,
  hostControl,
  HostControlError,
  unlockHost,
} from "./hostControl";
import {
  activeHost,
  pairHost,
  setHostAuthMode,
  unpairHost,
} from "./hostPairing";
import { clearHostNetwork, saveHostNetwork } from "./hostNetwork";
import { makeTestHost, sha256b64u, type TestHost } from "./testHost";
import { checkOpResult } from "./hostStatements";

/**
 * A stand-in private channel. It signs exactly as the real one does and hands
 * each frame to the same reply queue as the relay, marked `via: "channel"`, so
 * a test can tell which path an operation took. Its WebRTC and reply binding
 * are covered by `privateChannel.test.ts` and the Rust/Chromium integration test.
 */
let refuseChannel = false;
/** What the channel's mocked transfer methods report and record. */
let transferStatus = "delivered";
let transferCalls: unknown[][] = [];
/** Every channel the code under test asked to open, and what came of it. */
let opens: {
  hostId: string;
  iceServers: unknown;
  relayOnly: unknown;
  closed: boolean;
}[] = [];
/** While set, opening a channel waits for it. */
let holdOpen: Promise<void> | null = null;
vi.mock("./privateChannel", async (actual) => {
  const real = await actual<typeof import("./privateChannel")>();
  const auth = await import("./hostAuth");
  return {
    ...real,
    openPrivateChannel: async (options: {
      host: { id: string };
      key: Awaited<ReturnType<typeof auth.loadBrowserKey>>;
      dlg: string | null;
      iceServers?: RTCIceServer[];
      relayOnly?: boolean;
    }) => {
      const record = {
        hostId: options.host.id,
        iceServers: options.iceServers,
        relayOnly: options.relayOnly,
        closed: false,
      };
      opens.push(record);
      if (holdOpen) await holdOpen;
      if (refuseChannel) {
        sent.push({ type: "peer_challenge" });
        throw new real.PrivateChannelError("host refused", "refused");
      }
      let open = true;
      return {
        sendFiles: async (...args: unknown[]) => {
          transferCalls.push(["sendFiles", ...args]);
          return transferStatus;
        },
        pasteImage: async (...args: unknown[]) => {
          transferCalls.push(["pasteImage", ...args]);
          return transferStatus;
        },
        subscribeTerminal: async (...args: unknown[]) => {
          transferCalls.push(["subscribeTerminal", ...args]);
          return { sub: 7, close: () => {} };
        },
        settingsGet: async () => ({
          status: "executed",
          detail: null,
          body: { kind: "settings", launch: LAUNCH },
        }),
        peer: "test",
        get open() {
          return open;
        },
        close: () => {
          open = false;
          record.closed = true;
        },
        operation: async (
          op: Parameters<typeof auth.operationPayload>[0]["op"],
          session: string,
          args?: Record<string, unknown>,
          timing?: { lifetime?: number },
        ) => {
          const frame = await auth.signPayload(
            options.key,
            auth.operationPayload({
              hostId: options.host.id,
              key: options.key,
              dlg: options.dlg,
              op,
              session,
              args,
              lifetime: timing?.lifetime,
            }),
          );
          const request = { type: "signed_op", via: "channel", ...frame };
          sent.push(request);
          const next = replies.shift();
          if (!next) throw new Error("unexpected channel request");
          const reply = (await (await next(request)).json()) as {
            statement: unknown;
          };
          const outcome = await checkOpResult(
            options.host as never,
            { kid: options.key.kid, payloadB64u: frame.p },
            reply.statement,
          );
          return { ...outcome, body: null };
        },
      };
    },
  };
});

const CEREMONY = "c".repeat(32);
const DLG_NOT_USED = "0".repeat(32);

interface Sent {
  type: string;
  [key: string]: unknown;
}

let host: TestHost;
let sent: Sent[];
let replies: ((request: Sent) => Response | Promise<Response>)[];
let getAssertion: ReturnType<typeof vi.fn>;
const now = () => Math.floor(Date.now() / 1000);
const FUTURE = () => now() + 86_400;
const offeredExpiries = new Map<string, number>();

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

beforeEach(async () => {
  indexedDB.deleteDatabase("conversations-host-auth");
  localStorage.clear();
  const active = activeHost();
  if (active) unpairHost(active.id);
  host = await makeTestHost();
  closePrivateChannels();
  refuseChannel = false;
  transferStatus = "delivered";
  transferCalls = [];
  opens = [];
  holdOpen = null;
  sent = [];
  replies = [];
  offeredExpiries.clear();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_url: string, init: RequestInit) => {
      const request = JSON.parse(init.body as string) as Sent;
      sent.push(request);
      const next = replies.shift();
      if (!next) throw new Error("unexpected relay request " + request.type);
      return next(request);
    }),
  );
  // A stand-in authenticator: proves this page's UX and data handling only.
  // It performs no cryptography, so it says nothing about passkey security.
  getAssertion = vi.fn(async () => ({
    id: "cred",
    rawId: new Uint8Array([1, 2, 3]).buffer,
    type: "public-key",
    response: {
      authenticatorData: new Uint8Array([1]).buffer,
      clientDataJSON: new Uint8Array([2]).buffer,
      signature: new Uint8Array([3]).buffer,
      userHandle: null,
    },
    getClientExtensionResults: () => ({}),
  }));
  vi.stubGlobal("navigator", {
    ...navigator,
    credentials: { get: getAssertion, create: vi.fn() },
  });
  vi.stubGlobal("PublicKeyCredential", class {});
});

const pair = () => pairHost(host.descriptor, "laptop");

/** What an honest host signs in answer to a delegate_start. */
async function offerFor(
  request: Sent,
  override: Record<string, unknown> = {},
  signer: TestHost = host,
) {
  const key = await loadBrowserKey();
  const issuedAt = now();
  const payload = {
    v: 1,
    typ: "delegate_offer",
    host: host.id,
    nonce: request.nonce,
    ceremony: CEREMONY,
    browser_kid: key.kid,
    browser_pub: request.browser_pub,
    dlg: request.dlg,
    caps: [...(request.caps as string[])].sort(),
    issued_at: issuedAt,
    not_after: issuedAt + 86_400,
    webauthn_challenge: "AQID",
    rp_id: "localhost",
    ...override,
  };
  offeredExpiries.set(`${payload.ceremony}:${payload.dlg}`, payload.not_after);
  return json({
    type: "started",
    challenge: CEREMONY,
    options: {
      publicKey: {
        challenge: "AQID",
        rpId: "localhost",
        allowCredentials: [{ type: "public-key", id: "BAUG" }],
        userVerification: "required",
      },
    },
    statement: await signer.sign(payload),
  });
}

async function doneFor(request: Sent, override: Record<string, unknown> = {}) {
  const key = await loadBrowserKey();
  const notAfter = offeredExpiries.get(`${CEREMONY}:${request.dlg}`);
  if (notAfter === undefined)
    throw new Error("No fixture offer for this delegation");
  return json({
    type: "delegated",
    statement: await host.sign({
      v: 1,
      typ: "delegate_done",
      host: host.id,
      nonce: request.nonce,
      ceremony: CEREMONY,
      browser_kid: key.kid,
      dlg: request.dlg,
      caps: [...(request.caps as string[])].sort(),
      not_after: notAfter,
      ...override,
    }),
  });
}

async function resultFor(
  op: Sent,
  status: string,
  detail: string | null = null,
  override: Record<string, unknown> = {},
) {
  return json({
    type: "op",
    statement: await host.sign({
      v: 1,
      typ: "op_result",
      host: host.id,
      kid: op.k,
      payload_sha256: await sha256b64u(op.p as string),
      status,
      detail,
      ...override,
    }),
  });
}

/** A delegation for the current browser key, as a successful unlock would leave. */
async function unlocked() {
  const key = await loadBrowserKey();
  await saveDelegation({
    hostId: host.id,
    dlg: "f".repeat(32),
    kid: key.kid,
    notAfter: FUTURE(),
  });
}

describe("signed host control", () => {
  it("uses the locally enrolled raw browser key without requesting a passkey", async () => {
    await pairHost(host.descriptor, "laptop", "browser-key");
    replies.push((request) => resultFor(request, "executed"));
    await hostControl.reopenSession("s");
    expect(getAssertion).not.toHaveBeenCalled();
    expect(sent.map((request) => request.type)).toEqual(["signed_op"]);
    expect(
      JSON.parse(new TextDecoder().decode(fromBase64Url(sent[0].p as string)))
        .dlg,
    ).toBeNull();
  });
  it("opens a new channel when switching an existing pairing to a direct key", async () => {
    await pair();
    await unlocked();
    replies.push(
      (request) => resultFor(request, "executed"),
      (request) => resultFor(request, "executed"),
    );
    await hostControl.reopenSession("s");
    setHostAuthMode(host.id, "browser-key");
    await hostControl.reopenSession("s");
    expect(
      JSON.parse(new TextDecoder().decode(fromBase64Url(sent[0].p as string)))
        .dlg,
    ).not.toBeNull();
    expect(
      JSON.parse(new TextDecoder().decode(fromBase64Url(sent[1].p as string)))
        .dlg,
    ).toBeNull();
    expect(opens).toHaveLength(2);
    expect(opens[0].closed).toBe(true);
  });
  it("does not fall back to a passkey when direct enrollment is refused", async () => {
    await pairHost(host.descriptor, "laptop", "browser-key");
    refuseChannel = true;
    await expect(hostControl.reopenSession("s")).rejects.toThrow();
    expect(getAssertion).not.toHaveBeenCalled();
    expect(sent.some((request) => request.type === "delegate_start")).toBe(
      false,
    );
  });
  it("refuses to act, and sends nothing, when no host is paired", async () => {
    await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
      code: "not_paired",
    });
    expect(sent).toHaveLength(0);
  });

  it("does not prompt for a passive private read", async () => {
    await pair();
    await expect(
      hostControl.privateRead(host.id, async () => true),
    ).rejects.toMatchObject({ code: "locked" });
    expect(getAssertion).not.toHaveBeenCalled();
    expect(sent).toHaveLength(0);
  });

  it("unlocks with a passkey, then signs for the pinned host and reuses the delegation", async () => {
    await pair();
    replies.push(
      async (request) => offerFor(request),
      async (request) => {
        expect(request.type).toBe("delegate_finish");
        expect(request.challenge).toBe(CEREMONY);
        return doneFor(sent[0]);
      },
      async (request) => resultFor(request, "executed"),
      async (request) => resultFor(request, "executed"),
    );
    const started = await hostControl.startSession("claude");
    expect(started.sessionId).toMatch(/^[0-9a-f-]{36}$/);

    const [delegateStart, , signed] = sent;
    expect(delegateStart.type).toBe("delegate_start");
    expect(delegateStart.caps).toEqual([
      "history.read",
      "host.settings_read",
      "host.settings_write",
      "passkey.manage",
      "session.manage_sharing",
      "session.pr_comment",
      "session.reopen",
      "session.start",
      "session.write",
    ]);
    expect(delegateStart.nonce).toMatch(/^[0-9a-f]{32}$/);
    expect(atob(delegateStart.browser_pub as string)).toHaveLength(65);

    const payload = JSON.parse(
      new TextDecoder().decode(fromBase64Url(signed.p as string)),
    );
    expect(signed.type).toBe("signed_op");
    expect(payload).toMatchObject({
      aud: host.id,
      dlg: delegateStart.dlg,
      op: "session.start",
      sess: started.sessionId,
      args: { session: started.sessionId, agent: "claude" },
    });

    // Second operation: no new passkey prompt, same delegation, new jti.
    await hostControl.reopenSession(started.sessionId);
    expect(getAssertion).toHaveBeenCalledTimes(1);
    const second = JSON.parse(
      new TextDecoder().decode(fromBase64Url(sent[3].p as string)),
    );
    expect(second.dlg).toBe(delegateStart.dlg);
    expect(second.jti).not.toBe(payload.jti);
    expect(second.args).toEqual({ session: started.sessionId });
  });

  it("asks for a new passkey approval once the 24 hours have passed", async () => {
    await pair();
    const key = await loadBrowserKey();
    await saveDelegation({
      hostId: host.id,
      dlg: "e".repeat(32),
      kid: key.kid,
      notAfter: now() - 5,
    });
    replies.push(() => json({ type: "error", message: "no" }));
    await expect(hostControl.reopenSession("s")).rejects.toThrow("no");
    expect(sent[0].type).toBe("delegate_start");
  });

  it("does not sign or send anything when the passkey prompt is cancelled", async () => {
    await pair();
    getAssertion.mockRejectedValue(
      Object.assign(new Error("denied"), { name: "NotAllowedError" }),
    );
    replies.push((request) => offerFor(request));
    await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
      code: "cancelled",
    });
    expect(sent.map((s) => s.type)).toEqual(["delegate_start"]);
  });

  it("shows a refusal as-is and drops a delegation the host no longer accepts", async () => {
    await pair();
    await unlocked();
    replies.push((request) =>
      resultFor(
        request,
        "refused",
        "unknown, revoked or expired credential or delegation",
      ),
    );
    await expect(hostControl.reopenSession("s")).rejects.toThrow(
      /refused this: unknown, revoked or expired/,
    );
    expect(await getDelegation(host.id)).toBeNull();
  });

  it("reports interrupted and failed outcomes honestly, with no retry", async () => {
    await pair();
    await unlocked();
    replies.push((request) => resultFor(request, "uncertain"));
    await expect(hostControl.reopenSession("s")).rejects.toThrow(
      /will not repeat it/,
    );
    replies.push((request) =>
      resultFor(request, "failed", "failed earlier; not repeated"),
    );
    await expect(hostControl.reopenSession("s")).rejects.toThrow(
      /failed earlier/,
    );
    expect(sent).toHaveLength(2);
  });

  it("maps an offline host and a dropped connection to distinct, actionable errors", async () => {
    // Only the passkey bootstrap uses the relay; every control needs it first.
    await pair();
    replies.push(() =>
      json({ error: "That host is not connected.", code: "host_offline" }, 503),
    );
    await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
      code: "offline",
    });
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("network")));
    await expect(hostControl.reopenSession("s")).rejects.toThrow(
      /not retried and may or may not have run/,
    );
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(getAssertion).not.toHaveBeenCalled();
  });
});

describe("a hostile relay between this page and the host", () => {
  const unlockWith = async (
    mutate: (request: Sent) => Promise<Response>,
  ): Promise<unknown> => {
    await pair();
    replies.push(mutate);
    return hostControl.reopenSession("s").then(
      () => null,
      (error: unknown) => error,
    );
  };

  const expectRejectedBeforePasskey = (error: unknown) => {
    expect(error).toMatchObject({ code: "untrusted" });
    expect(getAssertion).not.toHaveBeenCalled();
    // Only the start was ever sent: nothing was finished or signed.
    expect(sent.map((s) => s.type)).toEqual(["delegate_start"]);
  };

  it("rejects an offer for a key the relay substituted, before the passkey prompt", async () => {
    // The relay swaps the browser's key for its own before the host sees it,
    // so the host's (honestly signed) offer names the attacker's key.
    const attacker = (await crypto.subtle.generateKey(
      { name: "ECDSA", namedCurve: "P-256" },
      true,
      ["sign"],
    )) as CryptoKeyPair;
    const raw = new Uint8Array(
      await crypto.subtle.exportKey("raw", attacker.publicKey),
    );
    const attackerPub = btoa(String.fromCharCode(...raw));
    const attackerKid = toBase64Url(
      new Uint8Array(await crypto.subtle.digest("SHA-256", raw)),
    );
    const error = await unlockWith((request) =>
      offerFor(request, { browser_pub: attackerPub, browser_kid: attackerKid }),
    );
    expectRejectedBeforePasskey(error);
  });

  it.each([
    ["scope", { caps: ["session.start", "session.reopen", "session.end"] }],
    ["delegation id", { dlg: DLG_NOT_USED }],
    ["nonce", { nonce: "f".repeat(32) }],
    ["host", { host: "x".repeat(43) }],
    ["passkey challenge", { webauthn_challenge: "AAAA" }],
    ["relying party", { rp_id: "evil.example" }],
    ["expiry", { not_after: now() + 86_400 * 30 }],
    [
      "old offer",
      { issued_at: now() - 7200, not_after: now() - 7200 + 86_400 },
    ],
    ["ceremony", { ceremony: "d".repeat(32) }],
    ["extra field", { extra: 1 }],
  ])(
    "rejects an offer with an altered %s before the passkey prompt",
    async (_name, override) => {
      const error = await unlockWith((request) => offerFor(request, override));
      expectRejectedBeforePasskey(error);
    },
  );

  it("rejects an offer signed by a different host key", async () => {
    const other = await makeTestHost();
    const error = await unlockWith((request) => offerFor(request, {}, other));
    expectRejectedBeforePasskey(error);
  });

  it("rejects options whose challenge the host did not sign", async () => {
    const error = await unlockWith(async (request) => {
      const response = (await (await offerFor(request)).json()) as {
        options: { publicKey: { challenge: string } };
      };
      response.options.publicKey.challenge = "BBBB";
      return json({ type: "started", challenge: CEREMONY, ...response });
    });
    expectRejectedBeforePasskey(error);
  });

  it("rejects an unsigned or malformed offer", async () => {
    for (const statement of [undefined, "x", { p: "a", s: "b" }]) {
      localStorage.clear();
      sent.length = 0;
      getAssertion.mockClear();
      const error = await unlockWith(async (request) => {
        const response = (await (await offerFor(request)).json()) as object;
        return json({ ...response, statement });
      });
      expectRejectedBeforePasskey(error);
    }
  });

  it("rejects a completion that is not the one for this request, and saves nothing", async () => {
    await pair();
    replies.push(
      (request) => offerFor(request),
      (request) => {
        void request;
        return doneFor(sent[0], { nonce: "0".repeat(32) });
      },
    );
    await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
      code: "untrusted",
    });
    expect(await getDelegation(host.id)).toBeNull();
  });

  it("rejects a forged success: an unsigned, mis-bound or replayed outcome", async () => {
    await pair();
    await unlocked();
    const rejects = async (
      reply: (op: Sent) => Promise<Response> | Response,
    ) => {
      replies.push(reply);
      await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
        code: "untrusted",
      });
    };
    // The relay invents a success.
    await rejects(() =>
      json({ type: "op", statement: { p: "e30", s: "AAAA" } }),
    );
    // An honest outcome, but for another operation (replayed old reply).
    await rejects(async (op) =>
      resultFor(op, "executed", null, { payload_sha256: "A".repeat(43) }),
    );
    // An honest outcome for a different signing key.
    await rejects(async (op) =>
      resultFor(op, "executed", null, { kid: "other" }),
    );
    // An outcome signed by a different host.
    const other = await makeTestHost();
    await rejects(async (op) =>
      json({
        type: "op",
        statement: await other.sign({
          v: 1,
          typ: "op_result",
          host: host.id,
          kid: op.k,
          payload_sha256: await sha256b64u(op.p as string),
          status: "executed",
          detail: null,
        }),
      }),
    );
  });

  it("refuses to act on a pairing that has no pinned key", async () => {
    localStorage.setItem(
      "conversations.hosts",
      JSON.stringify({
        hosts: [{ id: host.id, label: "old" }],
        active: host.id,
      }),
    );
    await expect(hostControl.reopenSession("s")).rejects.toMatchObject({
      code: "not_paired",
    });
    expect(sent).toHaveLength(0);
  });
});

const payloadOf = (request: Sent) =>
  JSON.parse(new TextDecoder().decode(fromBase64Url(request.p as string)));

describe("one unlock at a time", () => {
  it("keeps the offered expiry when passkey completion crosses a clock second", async () => {
    await pair();
    const issuedAt = Date.now();
    const clock = vi.spyOn(Date, "now").mockReturnValue(issuedAt);
    replies.push(
      (request) => offerFor(request),
      () => {
        clock.mockReturnValue(issuedAt + 1000);
        return doneFor(sent[0]);
      },
    );
    try {
      const delegation = await unlockHost(host.id);
      expect(delegation.notAfter).toBe(Math.floor(issuedAt / 1000) + 86_400);
      expect(await getDelegation(host.id)).toMatchObject({
        notAfter: delegation.notAfter,
      });
    } finally {
      clock.mockRestore();
    }
  });

  it("shares a single ceremony and passkey prompt between concurrent callers", async () => {
    await pair();
    replies.push(
      (request) => offerFor(request),
      () => doneFor(sent[0]),
    );
    const [a, b, c] = await Promise.all([
      unlockHost(host.id),
      unlockHost(host.id),
      unlockHost(host.id),
    ]);
    expect(b).toBe(a);
    expect(c).toBe(a);
    expect(sent.map((s) => s.type)).toEqual([
      "delegate_start",
      "delegate_finish",
    ]);
    expect(getAssertion).toHaveBeenCalledTimes(1);
    // Finished: a later unlock is a fresh ceremony again.
    replies.push(
      (request) => offerFor(request),
      () => doneFor(sent[2]),
    );
    await unlockHost(host.id);
    expect(sent.filter((s) => s.type === "delegate_start")).toHaveLength(2);
  });

  it("lets the first keystrokes wait on the same prompt instead of opening more", async () => {
    await pair();
    replies.push(
      (request) => offerFor(request),
      () => doneFor(sent[0]),
      (request) => resultFor(request, "executed"),
      (request) => resultFor(request, "executed"),
    );
    const channel = createInputChannel("s1", () => {});
    channel.send("a");
    channel.send("b");
    channel.send("c");
    // "a" waits on the unlock; "b" and "c" queue behind it as one frame.
    await vi.waitFor(() =>
      expect(sent.filter((s) => s.type === "signed_op")).toHaveLength(2),
    );
    expect(sent.filter((s) => s.type === "delegate_start")).toHaveLength(1);
    expect(getAssertion).toHaveBeenCalledTimes(1);
    channel.close();
  });

  it("records the scope the host actually delegated, which may be narrower", async () => {
    await pair();
    replies.push(
      (request) => offerFor(request),
      () => doneFor(sent[0], { caps: ["session.write"] }),
    );
    const delegation = await unlockHost(host.id);
    expect(delegation.caps).toEqual(["session.write"]);
    const parentKid = await sha256b64u("AQID");
    expect(delegation).toMatchObject({ parentKid });
    expect(await getDelegation(host.id)).toMatchObject({ parentKid });
  });

  it("does not save a delegation without the authenticator's credential id", async () => {
    await pair();
    getAssertion.mockResolvedValueOnce({
      id: "cred",
      rawId: new ArrayBuffer(0),
      type: "public-key",
      response: {
        authenticatorData: new Uint8Array([1]).buffer,
        clientDataJSON: new Uint8Array([2]).buffer,
        signature: new Uint8Array([3]).buffer,
        userHandle: null,
      },
      getClientExtensionResults: () => ({}),
    });
    replies.push(
      (request) => offerFor(request),
      () => doneFor(sent[0]),
    );
    await expect(unlockHost(host.id)).rejects.toMatchObject({
      code: "refused",
    });
    expect(await getDelegation(host.id)).toBeNull();
  });

  it("rejects a completion that WIDENS the scope or names nothing", async () => {
    for (const caps of [
      ["session.write", "session.end"],
      [],
      ["session.write", "session.write"],
    ]) {
      localStorage.clear();
      indexedDB.deleteDatabase("conversations-host-auth");
      sent.length = 0;
      await pair();
      replies.push(
        (request) => offerFor(request),
        () => doneFor(sent[0], { caps }),
      );
      await expect(unlockHost(host.id)).rejects.toMatchObject({
        code: "untrusted",
      });
      expect(await getDelegation(host.id)).toBeNull();
    }
  });
});

describe("signed writes", () => {
  it("signs a message with its session, text and a normal lifetime", async () => {
    await pair();
    await unlocked();
    replies.push((request) => resultFor(request, "executed"));
    await hostControl.sendReply("s1", "status please");
    const op = payloadOf(sent[0]);
    expect(op).toMatchObject({
      op: "session.reply",
      sess: "s1",
      args: { session: "s1", text: "status please" },
    });
    expect(op.exp - op.iat).toBe(60);
    // A prompt is private content: it travels on the private channel only.
    expect(sent[0].via).toBe("channel");
  });

  it("never sends a prompt, keystrokes or settings over the relay when the channel is refused", async () => {
    await pair();
    await unlocked();
    refuseChannel = true;
    await expect(
      hostControl.sendReply("s1", "secret prompt"),
    ).rejects.toMatchObject({
      code: "refused",
    });
    await expect(
      hostControl.updateHostSettings({ ...LAUNCH } as never),
    ).rejects.toMatchObject({ code: "refused" });
    const errors: unknown[] = [];
    const input = createInputChannel("s1", (e) => errors.push(e));
    input.send("secret keys");
    await vi.waitFor(() => expect(errors).toHaveLength(1));
    input.close();
    expect(sent.every((r) => r.type === "peer_challenge")).toBe(true);
    expect(JSON.stringify(sent)).not.toContain("secret");
  });

  it("sends dropped files and a pasted image only through the channel's transfer methods", async () => {
    await pair();
    await unlocked();
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    const text = new TextEncoder().encode("hello");
    const controller = new AbortController();
    await hostControl.pasteImage("s1", "image/png", png);
    await hostControl.sendFiles("s1", [{ name: "a.txt", bytes: text }], {
      signal: controller.signal,
    });
    expect(transferCalls).toEqual([
      ["pasteImage", "s1", "image/png", png, {}],
      [
        "sendFiles",
        "s1",
        [{ name: "a.txt", bytes: text }],
        { signal: controller.signal },
      ],
    ]);
    // Nothing went to the relay or the server: the channel carried it all.
    expect(sent).toEqual([]);
    expect(fetch).not.toHaveBeenCalled();
  });

  it.each([
    ["refused", "refused"],
    ["cancelled", "cancelled"],
    ["failed", "failed"],
  ] as const)(
    "reports a %s transfer as an error, never as delivered",
    async (status, code) => {
      await pair();
      await unlocked();
      transferStatus = status;
      await expect(
        hostControl.sendFiles("s1", [
          { name: "a.txt", bytes: new Uint8Array([1]) },
        ]),
      ).rejects.toMatchObject({ code });
      await expect(
        hostControl.pasteImage("s1", "image/png", new Uint8Array([1])),
      ).rejects.toMatchObject({ code });
    },
  );

  it("never claims nothing was pasted when a delivery failed part-way, and does not resend it", async () => {
    await pair();
    await unlocked();
    transferStatus = "failed";
    const error = await hostControl
      .sendFiles("s1", [
        { name: "a.txt", bytes: new Uint8Array([1]) },
        { name: "b.txt", bytes: new Uint8Array([2]) },
      ])
      .catch((e: unknown) => e);
    expect(error).toMatchObject({
      code: "failed",
      message: expect.stringMatching(/may have been pasted/),
    });
    expect((error as Error).message).not.toMatch(/Nothing was pasted/);
    expect(transferCalls).toHaveLength(1);
    expect(fetch).not.toHaveBeenCalled();
  });

  it("never reports an unconfirmed paste as delivered, and does not resend it", async () => {
    await pair();
    await unlocked();
    transferStatus = "uncertain";
    await expect(
      hostControl.sendFiles("s1", [
        { name: "a.txt", bytes: new Uint8Array([1]) },
      ]),
    ).rejects.toMatchObject({
      code: "uncertain",
      message: expect.stringMatching(/did not confirm/),
    });
    expect(transferCalls).toHaveLength(1);
    expect(sent).toEqual([]);
    expect(fetch).not.toHaveBeenCalled();
  });

  it("follows a terminal on the named host's channel", async () => {
    await pair();
    await unlocked();
    const frames: unknown[] = [];
    const sub = await hostControl.subscribeTerminal(host.id, "s1", {
      onFrame: (f) => frames.push(f),
      onEnd: () => {},
    });
    expect(sub.sub).toBe(7);
    expect(transferCalls[0][0]).toBe("subscribeTerminal");
    expect(transferCalls[0][1]).toBe("s1");
    const other = await makeTestHost();
    await expect(
      hostControl.subscribeTerminal(other.id, "s1", {
        onFrame: () => {},
        onEnd: () => {},
      }),
    ).rejects.toMatchObject({ code: "not_paired" });
  });

  it("cuts an over-long message at the signed limit without splitting a character", () => {
    const text = "é".repeat(3000);
    const cut = clampUtf8(text, 4096);
    expect(new TextEncoder().encode(cut).length).toBeLessThanOrEqual(4096);
    expect(new TextEncoder().encode(cut).length).toBe(4096);
    expect(clampUtf8("short", 4096)).toBe("short");
  });

  it("asks the host to end, resize and repaint with short-lived frames", async () => {
    await pair();
    await unlocked();
    replies.push(
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
    );
    await hostControl.endSession("s1");
    await hostControl.resizeSession("s1", 120, 40);
    await hostControl.repaintSession("s1");
    const [end, resize, repaint] = sent.map(payloadOf);
    expect(end).toMatchObject({ op: "session.end", args: { session: "s1" } });
    expect(end.exp - end.iat).toBe(60);
    expect(resize).toMatchObject({
      op: "session.resize",
      args: { session: "s1", cols: 120, rows: 40 },
    });
    expect(repaint).toMatchObject({ op: "session.repaint" });
    for (const frame of [resize, repaint])
      expect(frame.exp - frame.iat).toBe(20);
    // All three went on the private channel: nothing reached the relay.
    expect(fetch).not.toHaveBeenCalled();
    // Repaint is idempotent and unordered; resize has its own channel.
    expect(repaint.args).toEqual({ session: "s1" });
    expect(resize.args.ch).toMatch(/^[0-9a-f]{32}$/);
    expect(resize.args.seq).toBeGreaterThanOrEqual(1);
  });

  it("names exactly one session, one recipient and sorted capabilities when sharing", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "executed", "granted"));
    const recipient = "R".repeat(43);
    await hostControl.addGrant(
      "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
      ` ${recipient} `,
      ["session.write", "history.read", "session.write"],
    );
    expect(payloadOf(sent[0])).toMatchObject({
      op: "grant.add",
      sess: "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
      args: {
        session: "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
        recipient,
        caps: ["history.read", "session.write"],
      },
    });
  });

  it("reports a refused share as refused (no local authority), not as shared", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "refused", "no current grant"));
    await expect(
      hostControl.addGrant(
        "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
        "R".repeat(43),
      ),
    ).rejects.toThrow(/refused this: no current grant/);
  });
});

describe("signed keystroke input", () => {
  it("signs each frame with the browser key, in order, with rising sequence numbers", async () => {
    await pair();
    await unlocked();
    replies.push(
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
    );
    const errors: unknown[] = [];
    const channel = createInputChannel("s1", (e) => errors.push(e));
    channel.send("ls ");
    channel.send("-la\r"); // queued behind the first frame, sent as its own frame
    await vi.waitFor(() => expect(sent).toHaveLength(2));
    const [one, two] = sent.map(payloadOf);
    expect(atob(one.args.b64)).toBe("ls ");
    expect(atob(two.args.b64)).toBe("-la\r");
    expect(one.args.ch).toMatch(/^[0-9a-f]{32}$/);
    expect(two.args.ch).toBe(one.args.ch);
    expect([one.args.seq, two.args.seq]).toEqual([1, 2]);
    expect(one.op).toBe("session.input");
    expect(one.exp - one.iat).toBe(20);
    expect(sent.map((r) => r.via)).toEqual(["channel", "channel"]);
    expect(getAssertion).not.toHaveBeenCalled(); // typing never prompts for a passkey
    expect(errors).toEqual([]);
    channel.close();
  });

  it("gives every caller its own channel so two tabs cannot collide on a sequence number", async () => {
    await pair();
    await unlocked();
    for (let i = 0; i < 4; i++) replies.push((r) => resultFor(r, "executed"));
    const first = createInputChannel("s1", () => {});
    const second = createInputChannel("s1", () => {});
    first.send("a");
    await vi.waitFor(() => expect(sent).toHaveLength(1));
    second.send("b");
    await vi.waitFor(() => expect(sent).toHaveLength(2));
    first.send("c");
    await vi.waitFor(() => expect(sent).toHaveLength(3));
    const [a, b, c] = sent.map(payloadOf);
    expect(a.args.ch).not.toBe(b.args.ch);
    expect(c.args.ch).toBe(a.args.ch);
    expect([a.args.seq, b.args.seq, c.args.seq]).toEqual([1, 1, 2]);
    first.close();
    second.close();
  });

  it("does not let a resize or repaint in flight touch the input sequence", async () => {
    await pair();
    await unlocked();
    let release!: (r: Response) => void;
    // The first input's answer is delayed; a resize and repaint overtake it.
    replies.push(
      (r) =>
        new Promise<Response>((resolve) => (release = (x) => resolve(x))).then(
          () => resultFor(r, "executed"),
        ),
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
      (r) => resultFor(r, "executed"),
    );
    const channel = createInputChannel("s1", () => {});
    channel.send("x");
    await vi.waitFor(() => expect(sent).toHaveLength(1));
    await hostControl.resizeSession("s1", 100, 30);
    await hostControl.repaintSession("s1");
    await hostControl.resizeSession("s1", 110, 31);
    release(new Response());
    channel.send("y");
    await vi.waitFor(() => expect(sent).toHaveLength(5));
    const ops = sent.map(payloadOf);
    const inputs = ops.filter((o) => o.op === "session.input");
    const resizes = ops.filter((o) => o.op === "session.resize");
    expect(inputs.map((o) => o.args.seq)).toEqual([1, 2]);
    // Per tab, rising; the counter is module state shared with earlier tests.
    expect(resizes[1].args.seq).toBe(resizes[0].args.seq + 1);
    expect(resizes[0].args.ch).toBe(resizes[1].args.ch);
    expect(resizes[0].args.ch).not.toBe(inputs[0].args.ch);
    channel.close();
  });

  it("splits large input into bounded frames without cutting a character", async () => {
    await pair();
    await unlocked();
    for (let i = 0; i < 3; i++) replies.push((r) => resultFor(r, "executed"));
    const channel = createInputChannel("s1", () => {});
    channel.send("😀".repeat(1500)); // 3000 UTF-16 units
    await vi.waitFor(() => expect(sent.length).toBeGreaterThanOrEqual(3));
    let total = "";
    for (const frame of sent) {
      const bytes = fromBase64Url(payloadOf(frame).args.b64);
      expect(bytes.length).toBeLessThanOrEqual(4096);
      total += new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    }
    expect(total).toBe("😀".repeat(1500));
    channel.close();
  });

  it("reports a refusal once and drops what was queued behind it", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "refused", "no current grant"));
    const errors: Error[] = [];
    const channel = createInputChannel("s1", (e) => errors.push(e));
    channel.send("a");
    channel.send("b");
    await vi.waitFor(() => expect(errors).toHaveLength(1));
    expect(errors[0].message).toMatch(/refused this: no current grant/);
    expect(sent).toHaveLength(1); // "b" was not sent after the refusal
    channel.close();
  });

  it("sends nothing at all when no host is paired", async () => {
    const errors: Error[] = [];
    const channel = createInputChannel("s1", (e) => errors.push(e));
    channel.send("x");
    await vi.waitFor(() => expect(errors).toHaveLength(1));
    expect(errors[0]).toMatchObject({ code: "not_paired" });
    expect(sent).toHaveLength(0);
  });

  it("stops sending once closed", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "executed"));
    const channel = createInputChannel("s1", () => {});
    channel.send("a");
    channel.close();
    channel.send("b");
    await new Promise((r) => setTimeout(r, 30));
    expect(sent.length).toBeLessThanOrEqual(1);
  });
});

const LAUNCH = {
  claude_skip_permissions: true,
  opencode_auto: false,
  codex_sandbox_mode: "read-only",
  codex_approval_policy: null,
  default_launcher: "adk",
  adk_statusline: false,
};

describe("signed GitHub operations", () => {
  it("signs the exact repo, number, thread and body of a comment", async () => {
    await pair();
    await unlocked();
    replies.push((request) => resultFor(request, "executed"));
    await hostControl.commentOnPr("s1", {
      owner: "octo",
      repo: "repo",
      number: 7,
      body: "-x $(y)\nline two",
      in_reply_to: 99,
    });
    const op = payloadOf(sent[0]);
    expect(op).toMatchObject({
      op: "pr.comment",
      sess: "s1",
      args: {
        session: "s1",
        owner: "octo",
        repo: "repo",
        number: 7,
        body: "-x $(y)\nline two",
        in_reply_to: 99,
      },
    });
    expect(op.exp - op.iat).toBe(60);
  });

  it("signs a sync for ONE named pull request and omits an absent thread", async () => {
    await pair();
    await unlocked();
    replies.push(
      (request) => resultFor(request, "executed"),
      (request) => resultFor(request, "executed"),
    );
    await hostControl.syncPr("s1", { owner: "octo", repo: "repo", number: 7 });
    expect(payloadOf(sent[0])).toMatchObject({
      op: "pr.sync",
      args: { session: "s1", owner: "octo", repo: "repo", number: 7 },
    });
    await hostControl.commentOnPr("s1", {
      owner: "octo",
      repo: "repo",
      number: 7,
      body: "hi",
    });
    expect(payloadOf(sent[1]).args).not.toHaveProperty("in_reply_to");
  });

  it("shows the host's refusal when this key may not comment", async () => {
    await pair();
    await unlocked();
    replies.push((request) =>
      resultFor(request, "refused", "capability outside credential ceiling"),
    );
    await expect(
      hostControl.commentOnPr("s1", {
        owner: "o",
        repo: "r",
        number: 1,
        body: "x",
      }),
    ).rejects.toMatchObject({
      code: "refused",
      message: expect.stringContaining("capability outside credential ceiling"),
    });
  });
});

describe("signed host settings", () => {
  it("keeps explicit settings reads and writes on their captured host after selection changes", async () => {
    await pairHost(host.descriptor, "A", "browser-key");
    const capturedHostId = host.id;
    const other = await makeTestHost();
    await pairHost(other.descriptor, "B", "browser-key");
    expect(activeHost()?.id).toBe(other.id);
    await expect(hostControl.getHostSettings(capturedHostId)).resolves.toEqual(
      LAUNCH,
    );
    replies.push((request) => resultFor(request, "executed"));
    await hostControl.updateHostSettings(
      { ...LAUNCH } as never,
      capturedHostId,
    );
    expect(opens.map((opened) => opened.hostId)).toEqual([capturedHostId]);
    expect(payloadOf(sent[0])).toMatchObject({
      aud: capturedHostId,
      op: "settings.update",
      args: { launch: LAUNCH },
    });
    expect(activeHost()?.id).toBe(other.id);
    expect(getAssertion).not.toHaveBeenCalled();
  });

  it("refuses an unpaired explicit settings target instead of using the active machine", async () => {
    await pair();
    const unknown = await makeTestHost();
    await expect(hostControl.getHostSettings(unknown.id)).rejects.toMatchObject(
      { code: "not_paired" },
    );
    await expect(
      hostControl.updateHostSettings({ ...LAUNCH } as never, unknown.id),
    ).rejects.toMatchObject({ code: "not_paired" });
    expect(opens).toHaveLength(0);
    expect(sent).toHaveLength(0);
  });

  it("writes with a host-scoped frame that names no session", async () => {
    await pair();
    await unlocked();
    replies.push((request) => resultFor(request, "executed"));
    await hostControl.updateHostSettings({
      ...LAUNCH,
      claude_skip_permissions: false,
    } as never);
    const write = payloadOf(sent[0]);
    expect(write.sess).toBe("host");
    expect(write.args).toEqual({
      launch: { ...LAUNCH, claude_skip_permissions: false },
    });
    expect(write.args).not.toHaveProperty("session");
  });

  it("reads only over the private channel: no signed read crosses the relay", async () => {
    await expect(hostControl.getHostSettings()).rejects.toMatchObject({
      code: "not_paired",
    });
    expect(sent).toHaveLength(0);
    await pair();
    await unlocked();
    // The relay carries only the channel's signaling; refusing it fails the read.
    refuseChannel = true;
    await expect(hostControl.getHostSettings()).rejects.toMatchObject({
      code: "refused",
    });
    expect(sent.map((r) => r.type)).toEqual(["peer_challenge"]);
  });

});

describe("signed publication", () => {
  it("shares to a canonical audience on the publishing host and reports its state", async () => {
    await pair();
    await unlocked();
    replies.push(
      (request) => resultFor(request, "executed", "syncing"),
      (request) => resultFor(request, "executed", "deletion_pending"),
      (request) =>
        resultFor(
          request,
          "executed",
          JSON.stringify({
            state: "deletion_pending",
            rev: 2,
            audience: null,
            published: 0,
            pending: 0,
          }),
        ),
    );
    const shared = await hostControl.sharePublication(host.id, "s1", {
      visibility: "restricted",
      emails: [" Bob@X.test", "carol@x.test", "bob@x.test"],
    });
    expect(shared).toBe("syncing");
    expect(payloadOf(sent[0])).toMatchObject({
      aud: host.id,
      op: "publication.share",
      sess: "s1",
      args: {
        session: "s1",
        audience: {
          visibility: "restricted",
          emails: ["bob@x.test", "carol@x.test"],
        },
      },
    });
    expect(await hostControl.unsharePublication(host.id, "s1")).toBe(
      "deletion_pending",
    );
    expect(payloadOf(sent[1])).toMatchObject({
      op: "publication.unshare",
      args: { session: "s1" },
    });
    expect(await hostControl.publicationStatus(host.id, "s1")).toEqual({
      state: "deletion_pending",
      rev: 2,
      audience: null,
      published: 0,
      pending: 0,
      error: null,
    });
    expect(payloadOf(sent[2]).op).toBe("publication.status");
    expect(sent.map((r) => r.via)).toEqual(["channel", "channel", "channel"]);
  });

  it("reads the host's audience, revision and counts from its signed status", async () => {
    await pair();
    await unlocked();
    replies.push(
      (r) =>
        resultFor(
          r,
          "executed",
          JSON.stringify({
            state: "syncing",
            rev: 4,
            audience: {
              visibility: "restricted",
              emails: ["bob@x.test", "carol@x.test"],
            },
            published: 3,
            pending: 1,
          }),
        ),
      (r) =>
        resultFor(
          r,
          "executed",
          JSON.stringify({
            state: "error",
            rev: 4,
            audience: { visibility: "organization" },
            published: 3,
            pending: 0,
            error: "login expired",
          }),
        ),
    );
    expect(await hostControl.publicationStatus(host.id, "s1")).toEqual({
      state: "syncing",
      rev: 4,
      audience: {
        visibility: "restricted",
        emails: ["bob@x.test", "carol@x.test"],
      },
      published: 3,
      pending: 1,
      error: null,
    });
    expect(await hostControl.publicationStatus(host.id, "s1")).toMatchObject({
      state: "error",
      audience: { visibility: "organization" },
      error: "login expired",
    });
  });

  it.each([
    ["a bare phase", "shared"],
    [
      "no audience field",
      JSON.stringify({ state: "shared", rev: 1, published: 1, pending: 0 }),
    ],
    [
      "an unknown state",
      JSON.stringify({
        state: "fine",
        rev: 1,
        audience: null,
        published: 0,
        pending: 0,
      }),
    ],
    [
      "a negative count",
      JSON.stringify({
        state: "shared",
        rev: 1,
        audience: null,
        published: -1,
        pending: 0,
      }),
    ],
    [
      "a fractional revision",
      JSON.stringify({
        state: "shared",
        rev: 1.5,
        audience: null,
        published: 0,
        pending: 0,
      }),
    ],
    [
      "an unknown visibility",
      JSON.stringify({
        state: "shared",
        rev: 1,
        audience: { visibility: "public" },
        published: 0,
        pending: 0,
      }),
    ],
    [
      "restricted without emails",
      JSON.stringify({
        state: "shared",
        rev: 1,
        audience: { visibility: "restricted" },
        published: 0,
        pending: 0,
      }),
    ],
    [
      "extra audience fields",
      JSON.stringify({
        state: "shared",
        rev: 1,
        audience: { visibility: "organization", emails: ["x@y.z"] },
        published: 0,
        pending: 0,
      }),
    ],
  ])(
    "treats a status with %s as untrusted, never as a default",
    async (_, detail) => {
      await pair();
      await unlocked();
      replies.push((r) => resultFor(r, "executed", detail));
      await expect(
        hostControl.publicationStatus(host.id, "s1"),
      ).rejects.toMatchObject({ code: "untrusted" });
    },
  );

  it("refuses to act for a host this browser has not paired", async () => {
    await pair();
    await unlocked();
    const other = await makeTestHost();
    await expect(
      hostControl.unsharePublication(other.id, "s1"),
    ).rejects.toBeInstanceOf(Error);
    expect(sent).toHaveLength(0);
  });

  it("treats an unknown state from the host as a failure, not as shared", async () => {
    await pair();
    await unlocked();
    replies.push((request) =>
      resultFor(request, "executed", "everything is fine"),
    );
    await expect(
      hostControl.publicationStatus(host.id, "s1"),
    ).rejects.toMatchObject({ code: "untrusted" });
  });
});

describe("content-bearing controls", () => {
  it("travel on the private channel: start, reopen, PR target and comment, images and files", async () => {
    await pair();
    await unlocked();
    for (let i = 0; i < 4; i++) replies.push((r) => resultFor(r, "executed"));
    await hostControl.startSession("claude");
    await hostControl.syncPr("s1", { owner: "octo", repo: "repo", number: 7 });
    await hostControl.commentOnPr("s1", {
      owner: "octo",
      repo: "repo",
      number: 7,
      body: "looks good",
    });
    await hostControl.pasteImage("s1", "image/png", new Uint8Array([1]));
    await hostControl.sendFiles("s1", [
      { name: "notes.txt", bytes: new Uint8Array([2]) },
    ]);
    await hostControl.reopenSession("s1");
    expect(sent.map((r) => [payloadOf(r).op, r.via ?? "relay"])).toEqual([
      ["session.start", "channel"],
      ["pr.sync", "channel"],
      ["pr.comment", "channel"],
      ["session.reopen", "channel"],
    ]);
    expect(transferCalls.map((c) => c[0])).toEqual(["pasteImage", "sendFiles"]);
  });

  it("are never retried over the relay when the private channel is refused", async () => {
    await pair();
    await unlocked();
    refuseChannel = true;
    const attempts = [
      hostControl.startSession("claude"),
      hostControl.reopenSession("s1"),
      hostControl.syncPr("s1", {
        owner: "secret-owner",
        repo: "secret-repo",
        number: 7,
      }),
      hostControl.commentOnPr("s1", {
        owner: "secret-owner",
        repo: "secret-repo",
        number: 7,
        body: "secret comment",
      }),
      hostControl.pasteImage("s1", "image/png", new Uint8Array([1])),
      hostControl.sendFiles("s1", [
        { name: "secret-name.txt", bytes: new Uint8Array([2]) },
      ]),
    ];
    for (const result of await Promise.allSettled(attempts)) {
      expect(result).toMatchObject({
        status: "rejected",
        reason: { code: "refused" },
      });
    }
    expect(sent.every((r) => r.type === "peer_challenge")).toBe(true);
    expect(JSON.stringify(sent)).not.toContain("secret");
  });
});

describe("lifecycle, grant and publication controls", () => {
  it("travel on the private channel too", async () => {
    await pair();
    await unlocked();
    for (let i = 0; i < 4; i++)
      replies.push((r) => resultFor(r, "executed", "syncing"));
    await hostControl.endSession("s1");
    await hostControl.addGrant("s1", "R".repeat(43));
    await hostControl.removeGrant("s1", "R".repeat(43), ["session.write"]);
    await hostControl.sharePublication(host.id, "s1", {
      visibility: "organization",
    });
    expect(sent.map((r) => [payloadOf(r).op, r.via ?? "relay"])).toEqual([
      ["session.end", "channel"],
      ["grant.add", "channel"],
      ["grant.remove", "channel"],
      ["publication.share", "channel"],
    ]);
  });

  it("are never retried over the relay when the private channel is refused", async () => {
    await pair();
    await unlocked();
    refuseChannel = true;
    const attempts = [
      hostControl.endSession("s1"),
      hostControl.resizeSession("s1", 120, 40),
      hostControl.repaintSession("s1"),
      hostControl.addGrant("s1", "R".repeat(43)),
      hostControl.removeGrant("s1", "R".repeat(43), ["session.write"]),
      hostControl.sharePublication(host.id, "s1", {
        visibility: "restricted",
        emails: ["secret-reader@x.test"],
      }),
      hostControl.unsharePublication(host.id, "s1"),
      hostControl.publicationStatus(host.id, "s1"),
    ];
    for (const result of await Promise.allSettled(attempts)) {
      expect(result).toMatchObject({
        status: "rejected",
        reason: { code: "refused" },
      });
    }
    expect(sent.every((r) => r.type === "peer_challenge")).toBe(true);
    expect(JSON.stringify(sent)).not.toContain("secret");
  });

  it("acts on the host it is told to, and refuses a host this browser has not paired", async () => {
    await pair();
    await unlocked();
    const other = await makeTestHost();
    await expect(
      hostControl.sendReply("s1", "hello", other.id),
    ).rejects.toMatchObject({ code: "not_paired" });
    await expect(
      hostControl.addGrant("s1", "R".repeat(43), ["session.write"], other.id),
    ).rejects.toMatchObject({ code: "not_paired" });
    await expect(
      hostControl.privateRead(other.id, async () => "read"),
    ).rejects.toMatchObject({ code: "not_paired" });
    expect(sent).toHaveLength(0);
  });
});

describe("removing a session grant", () => {
  it("names exactly one session, one recipient and the sorted capabilities removed", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "executed", "removed"));
    const recipient = "R".repeat(43);
    await hostControl.removeGrant(
      "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
      ` ${recipient} `,
      ["session.write", "history.read"],
    );
    expect(payloadOf(sent[0])).toMatchObject({
      op: "grant.remove",
      sess: "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
      args: {
        session: "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
        recipient,
        caps: ["history.read", "session.write"],
      },
    });
  });

  it("reports a refused removal as refused, not as removed", async () => {
    await pair();
    await unlocked();
    replies.push((r) => resultFor(r, "refused", "no current grant"));
    await expect(
      hostControl.removeGrant(
        "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11",
        "R".repeat(43),
        ["session.write"],
      ),
    ).rejects.toMatchObject({ code: "refused" });
  });
});

describe("this browser's network settings for a host", () => {
  const TURN = {
    iceServers: [
      {
        urls: "turn:turn.example.com:3478?transport=udp",
        username: "turnuser",
        credential: "turnsecretmark",
      },
    ],
    iceTransportPolicy: "relay",
  };
  const follow = (hostId = host.id) =>
    hostControl.subscribeTerminal(hostId, "s1", {
      onFrame: () => {},
      onEnd: () => {},
    });

  it("opens a channel with no ICE servers and no relay policy by default", async () => {
    await pair();
    await unlocked();
    await follow();
    expect(opens).toEqual([
      { hostId: host.id, iceServers: [], relayOnly: false, closed: false },
    ]);
  });

  it("opens the channel with the host's saved ICE servers and relay-only policy", async () => {
    await pair();
    await unlocked();
    saveHostNetwork(host.id, TURN);
    await follow();
    expect(opens).toHaveLength(1);
    expect(opens[0].iceServers).toEqual(TURN.iceServers);
    expect(opens[0].relayOnly).toBe(true);
  });

  it("closes the cached channel when the settings are saved or cleared, and opens a new one under them", async () => {
    await pair();
    await unlocked();
    await follow();
    await follow();
    expect(opens).toHaveLength(1);
    saveHostNetwork(host.id, TURN);
    await vi.waitFor(() => expect(opens[0].closed).toBe(true));
    await follow();
    expect(opens).toHaveLength(2);
    expect(opens[1].relayOnly).toBe(true);
    clearHostNetwork(host.id);
    await vi.waitFor(() => expect(opens[1].closed).toBe(true));
    await follow();
    expect(opens).toHaveLength(3);
    expect(opens[2]).toMatchObject({ iceServers: [], relayOnly: false });
  });

  it("never uses a channel that finished opening under old settings, and keeps the new one", async () => {
    await pair();
    await unlocked();
    let release!: () => void;
    holdOpen = new Promise((resolve) => (release = resolve));
    const old = follow();
    await vi.waitFor(() => expect(opens).toHaveLength(1));
    holdOpen = null;
    saveHostNetwork(host.id, TURN);
    await follow();
    expect(opens).toHaveLength(2);
    expect(transferCalls).toHaveLength(1);
    release();
    await expect(old).rejects.toMatchObject({ code: "transport" });
    expect(opens[0].closed).toBe(true);
    // The read ran once, on the new channel; the old one was not retried.
    expect(transferCalls).toHaveLength(1);
    await follow();
    expect(opens).toHaveLength(2);
    expect(opens[1].closed).toBe(false);
  });

  it("does not start a connection under settings that changed while it was preparing", async () => {
    await pair();
    // No delegation yet: preparing the channel waits on the unlock, which is
    // held at the host's offer until the settings have changed.
    let held = false;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    replies.push(
      async (request) => {
        held = true;
        await gate;
        return offerFor(request);
      },
      async () => doneFor(sent[0]),
    );
    const pending = hostControl.startSession("claude");
    await vi.waitFor(() => expect(held).toBe(true));
    saveHostNetwork(host.id, TURN);
    release();
    await expect(pending).rejects.toMatchObject({ code: "transport" });
    expect(opens).toEqual([]);
    expect(transferCalls).toEqual([]);
  });

  it("leaves another host's channel alone", async () => {
    await pair();
    await unlocked();
    const other = await makeTestHost();
    pairHost(other.descriptor, "desktop");
    await follow();
    saveHostNetwork(other.id, TURN);
    await Promise.resolve();
    expect(opens[0].closed).toBe(false);
    await follow();
    expect(opens).toHaveLength(1);
  });

  it("fails closed on corrupt saved settings: no channel, and no credential in the error", async () => {
    await pair();
    await unlocked();
    localStorage.setItem(
      "conversations.hostNetwork",
      JSON.stringify({
        [host.id]: {
          rev: 1,
          network: {
            iceServers: [{ urls: "turn:x", credential: "turnsecretmark" }],
            iceTransportPolicy: "relay",
          },
        },
      }),
    );
    const error = await follow().catch((e: unknown) => e);
    expect(error).toBeInstanceOf(HostControlError);
    expect(String((error as Error).message)).not.toContain("turnsecretmark");
    expect(opens).toHaveLength(0);
    expect(transferCalls).toHaveLength(0);
  });
});
