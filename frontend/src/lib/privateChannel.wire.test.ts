import { afterEach, describe, expect, it, vi } from "vitest";
import { fromBase64Url, toBase64Url, type BrowserKey } from "./hostAuth";
import type { PinnedHost } from "./hostStatements";
import {
  channel,
  MAX_CHUNK_BYTES,
  MAX_IN_FLIGHT,
  PrivateChannelError,
  type PrivateChannel,
  type TerminalEnd,
  type TerminalFrame,
  type TerminalLease,
} from "./privateChannel";

const SESSION = "11111111-2222-3333-4444-555555555555";
const PEER = "0123456789abcdef0123456789abcdef";
const HOST_DOMAIN = "conversations-host/v1\n";

async function sha(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(
    await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>),
  );
}

const hex = (bytes: Uint8Array) =>
  Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

type Sent = Record<string, unknown>;

/** The host end of a data channel: signs replies the way the host does. */
class FakeHost extends EventTarget {
  readyState: RTCDataChannelState = "open";
  bufferedAmount = 0;
  bufferedAmountLowThreshold = 0;
  sent: Sent[] = [];
  /** Replies held back until released, when set. */
  held: (() => void)[] | null = null;
  pinned!: PinnedHost;
  key!: BrowserKey;
  private signing!: CryptoKey;
  /** How the host answers each operation (by op name). */
  answer: (op: Record<string, unknown>) => {
    status: string;
    detail?: string | null;
    body?: unknown;
  } = () => ({ status: "executed" });

  static async create(): Promise<FakeHost> {
    const host = new FakeHost();
    const pair = (await crypto.subtle.generateKey("Ed25519", true, [
      "sign",
      "verify",
    ])) as CryptoKeyPair;
    host.signing = pair.privateKey;
    const raw = new Uint8Array(
      await crypto.subtle.exportKey("raw", pair.publicKey),
    );
    host.pinned = { id: toBase64Url(await sha(raw)), key: toBase64Url(raw) };
    const browser = (await crypto.subtle.generateKey(
      { name: "ECDSA", namedCurve: "P-256" },
      true,
      ["sign", "verify"],
    )) as CryptoKeyPair;
    const publicRaw = new Uint8Array(
      await crypto.subtle.exportKey("raw", browser.publicKey),
    );
    host.key = {
      privateKey: browser.privateKey,
      publicRaw,
      kid: toBase64Url(await sha(publicRaw)),
    };
    return host;
  }

  open(): PrivateChannel {
    return channel(
      { close: () => undefined, getStats: async () => new Map() as never },
      this as unknown as RTCDataChannel,
      { host: this.pinned, key: this.key, dlg: null },
      PEER,
    );
  }

  send(text: string) {
    const message = JSON.parse(text) as Sent;
    this.sent.push(message);
    if (message.t === "op") void this.reply(message);
  }

  close() {
    if (this.readyState === "closed") return;
    this.readyState = "closed";
    this.dispatchEvent(new Event("close"));
  }

  deliver(message: Sent) {
    this.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(message) }),
    );
  }

  ops(): Record<string, unknown>[] {
    return this.sent
      .filter((m) => m.t === "op")
      .map((m) =>
        JSON.parse(new TextDecoder().decode(fromBase64Url(m.p as string))),
      );
  }

  chunks(): Sent[] {
    return this.sent.filter((m) => m.t === "chunk");
  }

  private async reply(message: Sent) {
    const payload = fromBase64Url(message.p as string);
    const op = JSON.parse(new TextDecoder().decode(payload));
    const { status, detail = null, body = null } = this.answer(op);
    const statement = new TextEncoder().encode(
      JSON.stringify({
        v: 1,
        typ: "channel_reply",
        host: this.pinned.id,
        peer: PEER,
        kid: this.key.kid,
        request: toBase64Url(await sha(payload)),
        status,
        detail,
        body,
      }),
    );
    const signed = new Uint8Array(HOST_DOMAIN.length + statement.length);
    signed.set(new TextEncoder().encode(HOST_DOMAIN));
    signed.set(statement, HOST_DOMAIN.length);
    const s = new Uint8Array(
      await crypto.subtle.sign("Ed25519", this.signing, signed),
    );
    const send = () =>
      this.deliver({
        t: "reply",
        id: message.id,
        statement: { p: toBase64Url(statement), s: toBase64Url(s) },
      });
    if (this.held) this.held.push(send);
    else send();
  }
}

const transferOf = (op: Record<string, unknown>) =>
  (op.args as { transfer: string }).transfer;

afterEach(() => {
  vi.useRealTimers();
});

describe("private file and image transfer", () => {
  it("signs the exact names, sizes and hashes, then sends the bytes in order on the channel only", async () => {
    const host = await FakeHost.create();
    const fetchSpy = vi.spyOn(globalThis, "fetch");
    const big = new Uint8Array(MAX_CHUNK_BYTES * 2 + 5).map((_, i) => i % 251);
    const small = new TextEncoder().encode("notes");
    const progress: number[] = [];
    const sent = host.open().sendFiles(
      SESSION,
      [
        { name: "big.bin", bytes: big },
        { name: "notes.txt", bytes: small },
      ],
      { onProgress: (n) => progress.push(n) },
    );
    await vi.waitFor(() => expect(host.chunks()).toHaveLength(4));
    const [op] = host.ops();
    expect(op.op).toBe("session.send_files");
    expect(op.args).toEqual({
      session: SESSION,
      transfer: transferOf(op),
      files: [
        { name: "big.bin", size: big.length, sha256: hex(await sha(big)) },
        { name: "notes.txt", size: 5, sha256: hex(await sha(small)) },
      ],
    });
    expect(host.chunks().map((c) => [c.transfer, c.file, c.offset])).toEqual([
      [transferOf(op), 0, 0],
      [transferOf(op), 0, MAX_CHUNK_BYTES],
      [transferOf(op), 0, MAX_CHUNK_BYTES * 2],
      [transferOf(op), 1, 0],
    ]);
    const joined = host
      .chunks()
      .filter((c) => c.file === 0)
      .flatMap((c) => [...fromBase64Url(c.data as string)]);
    expect(new Uint8Array(joined)).toEqual(big);
    expect(progress.at(-1)).toBe(big.length + 5);
    host.deliver({
      t: "transfer",
      transfer: transferOf(op),
      status: "delivered",
    });
    await expect(sent).resolves.toBe("delivered");
    expect(fetchSpy).not.toHaveBeenCalled();
    fetchSpy.mockRestore();
  });

  it("pastes an image as one signed file of its exact size and hash", async () => {
    const host = await FakeHost.create();
    const png = new Uint8Array([137, 80, 78, 71, 1, 2, 3]);
    const pasted = host.open().pasteImage(SESSION, "image/png", png);
    await vi.waitFor(() => expect(host.chunks()).toHaveLength(1));
    const [op] = host.ops();
    expect(op.op).toBe("session.paste_image");
    expect(op.args).toEqual({
      session: SESSION,
      transfer: transferOf(op),
      mime: "image/png",
      size: png.length,
      sha256: hex(await sha(png)),
    });
    host.deliver({
      t: "transfer",
      transfer: transferOf(op),
      status: "uncertain",
    });
    await expect(pasted).resolves.toBe("uncertain");
  });

  it("sends no bytes when the host refuses the signed transfer", async () => {
    const host = await FakeHost.create();
    host.answer = () => ({ status: "refused", detail: "not granted" });
    await expect(
      host
        .open()
        .sendFiles(SESSION, [{ name: "a.txt", bytes: new Uint8Array([1]) }]),
    ).rejects.toThrow("not granted");
    expect(host.chunks()).toHaveLength(0);
  });

  it("cancels on abort and reports the host's outcome", async () => {
    const host = await FakeHost.create();
    host.bufferedAmount = 1 << 30; // the channel is backed up
    const abort = new AbortController();
    const sent = host
      .open()
      .sendFiles(
        SESSION,
        [{ name: "a.bin", bytes: new Uint8Array(MAX_CHUNK_BYTES * 3) }],
        { signal: abort.signal },
      );
    await vi.waitFor(() => expect(host.ops()).toHaveLength(1));
    const transfer = transferOf(host.ops()[0]);
    abort.abort();
    await vi.waitFor(() =>
      expect(host.sent.filter((m) => m.t === "cancel")).toEqual([
        { t: "cancel", transfer },
      ]),
    );
    host.deliver({ t: "transfer", transfer, status: "cancelled" });
    host.bufferedAmount = 0;
    host.dispatchEvent(new Event("bufferedamountlow"));
    await expect(sent).resolves.toBe("cancelled");
    expect(host.chunks()).toHaveLength(0);
  });

  it("reports failed, not uncertain, when the channel closes before every byte left", async () => {
    const host = await FakeHost.create();
    host.bufferedAmount = 1 << 30;
    const sent = host
      .open()
      .sendFiles(SESSION, [
        { name: "a.bin", bytes: new Uint8Array(MAX_CHUNK_BYTES * 2) },
      ]);
    await vi.waitFor(() => expect(host.ops()).toHaveLength(1));
    host.close();
    await expect(sent).resolves.toBe("failed");
  });

  it("reports uncertain when the channel closes after every byte left", async () => {
    const host = await FakeHost.create();
    const sent = host
      .open()
      .sendFiles(SESSION, [{ name: "a.txt", bytes: new Uint8Array([1]) }]);
    await vi.waitFor(() => expect(host.chunks()).toHaveLength(1));
    host.close();
    await expect(sent).resolves.toBe("uncertain");
  });

  it("closes the channel on a malformed transfer outcome", async () => {
    const host = await FakeHost.create();
    host.open();
    host.deliver({
      t: "transfer",
      transfer: "x",
      status: "delivered",
      extra: 1,
    });
    expect(host.readyState).toBe("closed");
  });
});

/** A terminal piece the way the host sends it. */
const piece = (
  sub: number,
  seq: number,
  kind: "snapshot" | "update" | "end",
  data: string,
  fin = true,
  end: "expired" | "withdrawn" | "gone" | "unsubscribed" = "gone",
) => ({
  t: "terminal",
  sub,
  seq,
  kind,
  cols: kind === "end" ? 0 : 80,
  rows: kind === "end" ? 0 : 24,
  data: toBase64Url(new TextEncoder().encode(data)),
  fin,
  ...(kind === "end" ? { end } : {}),
});

const text = (frame: TerminalFrame) => new TextDecoder().decode(frame.data);

describe("private terminal follow", () => {
  async function following(host: FakeHost, lease?: TerminalLease) {
    host.answer = (op) =>
      op.op === "terminal.subscribe"
        ? { status: "executed", body: { kind: "terminal", sub: 1 } }
        : { status: "refused" };
    const frames: TerminalFrame[] = [];
    const ends: TerminalEnd[] = [];
    const sub = await host.open().subscribeTerminal(
      SESSION,
      {
        onFrame: (f) => frames.push(f),
        onEnd: (end) => ends.push(end),
      },
      lease,
    );
    return { sub, frames, ends };
  }

  it("signs a subscribe for exactly one session and renders the snapshot then each update once", async () => {
    const host = await FakeHost.create();
    const { sub, frames, ends } = await following(host);
    expect(sub.sub).toBe(1);
    expect(host.ops()[0].op).toBe("terminal.subscribe");
    expect(host.ops()[0].args).toEqual({ session: SESSION });
    host.deliver(piece(1, 0, "snapshot", "scr", false));
    host.deliver(piece(1, 1, "snapshot", "een"));
    host.deliver(piece(1, 2, "update", "one"));
    host.deliver(piece(1, 3, "update", "two"));
    expect(frames.map((f) => [f.kind, text(f)])).toEqual([
      ["snapshot", "screen"],
      ["update", "one"],
      ["update", "two"],
    ]);
    host.deliver(piece(1, 4, "end", ""));
    expect(ends).toEqual([
      { reason: "gone", detail: "The terminal ended on your host." },
    ]);
    expect(host.readyState).toBe("open");
  });

  it("stops on a repeated or skipped piece and tells the host to stop", async () => {
    const host = await FakeHost.create();
    const { frames, ends } = await following(host);
    host.deliver(piece(1, 0, "snapshot", "s"));
    host.deliver(piece(1, 0, "snapshot", "s"));
    expect(frames).toHaveLength(1);
    expect(ends).toEqual([
      {
        reason: "protocol",
        detail: "The terminal stream skipped or repeated a frame.",
      },
    ]);
    expect(host.sent.filter((m) => m.t === "unsubscribe")).toEqual([
      { t: "unsubscribe", sub: 1 },
    ]);
    // Later pieces of the left stream render nothing.
    host.deliver(piece(1, 1, "update", "late"));
    expect(frames).toHaveLength(1);
  });

  it("never renders an update before the screen", async () => {
    const host = await FakeHost.create();
    const { frames, ends } = await following(host);
    host.deliver(piece(1, 0, "update", "x"));
    expect(frames).toHaveLength(0);
    expect(ends.map((e) => e.reason)).toEqual(["protocol"]);
  });

  it("renews the lease with a fresh signed subscribe and ends when renewal is refused", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const host = await FakeHost.create();
    const { ends } = await following(host);
    await vi.advanceTimersByTimeAsync(31_000);
    await vi.waitFor(() => expect(host.ops()).toHaveLength(2));
    const [first, second] = host.ops();
    expect(second.op).toBe("terminal.subscribe");
    expect(second.jti).not.toBe(first.jti);
    expect(ends).toHaveLength(0);
    host.answer = () => ({ status: "refused", detail: "revoked" });
    await vi.advanceTimersByTimeAsync(31_000);
    await vi.waitFor(() =>
      expect(ends).toEqual([{ reason: "denied", detail: "revoked" }]),
    );
    expect(host.sent.filter((m) => m.t === "unsubscribe")).toHaveLength(1);
    // Denied: nothing is retried, however long the page stays open.
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(host.ops()).toHaveLength(3);
  });

  it("signs the lease it asks for and renews at half of it", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const host = await FakeHost.create();
    await following(host, { leaseSecs: 4 });
    const [first] = host.ops();
    expect((first.exp as number) - (first.iat as number)).toBe(4);
    await vi.advanceTimersByTimeAsync(1_900);
    expect(host.ops()).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(200);
    await vi.waitFor(() => expect(host.ops()).toHaveLength(2));
    const second = host.ops()[1];
    expect((second.exp as number) - (second.iat as number)).toBe(4);
  });

  it("refuses a second follow of a followed terminal without signing, so it cannot shorten the first one's lease", async () => {
    const host = await FakeHost.create();
    host.answer = () => ({
      status: "executed",
      body: { kind: "terminal", sub: 1 },
    });
    const channel = host.open();
    const ends: TerminalEnd[] = [];
    const handlers = {
      onFrame: () => {},
      onEnd: (e: TerminalEnd) => ends.push(e),
    };
    await channel.subscribeTerminal(SESSION, handlers);
    const again = [
      channel.subscribeTerminal(SESSION, handlers, { leaseSecs: 4 }),
      channel.subscribeTerminal(SESSION, handlers),
    ];
    for (const attempt of again) {
      await expect(attempt).rejects.toMatchObject({ code: "refused" });
    }
    expect(host.ops()).toHaveLength(1);
    expect(ends).toEqual([]);
  });

  it("refuses a lease outside the host's bounds without signing anything", async () => {
    const host = await FakeHost.create();
    for (const leaseSecs of [0, 1, 1.5, 61, Number.NaN]) {
      await expect(following(host, { leaseSecs })).rejects.toThrow(
        PrivateChannelError,
      );
    }
    expect(host.ops()).toHaveLength(0);
  });

  it("does not renew when asked not to, and reports the lapsed lease as expired", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const host = await FakeHost.create();
    const { ends } = await following(host, { leaseSecs: 4, autoRenew: false });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(host.ops()).toHaveLength(1);
    host.deliver(piece(1, 0, "end", "", true, "expired"));
    expect(ends).toEqual([
      {
        reason: "expired",
        detail: "The terminal lease lapsed. Follow it again to continue.",
      },
    ]);
  });

  it("reports authority the host withdrew as denied", async () => {
    const host = await FakeHost.create();
    const { ends } = await following(host);
    host.deliver(piece(1, 0, "snapshot", "s"));
    host.deliver(piece(1, 1, "end", "", true, "withdrawn"));
    expect(ends).toEqual([
      {
        reason: "denied",
        detail:
          "Your host no longer allows this view. Unlock it again to continue.",
      },
    ]);
  });

  it("closes the channel on an end without a cause or a cause on a frame", async () => {
    for (const bad of [
      { ...piece(1, 0, "end", ""), end: undefined },
      { ...piece(1, 0, "snapshot", "s"), end: "gone" },
      { ...piece(1, 0, "end", ""), end: "later" },
    ]) {
      const host = await FakeHost.create();
      await following(host);
      host.deliver(JSON.parse(JSON.stringify(bad)));
      expect(host.readyState).toBe("closed");
    }
  });

  it("holds pieces that arrive before the subscribe reply is verified", async () => {
    const host = await FakeHost.create();
    host.answer = () => ({
      status: "executed",
      body: { kind: "terminal", sub: 2 },
    });
    const frames: TerminalFrame[] = [];
    const channel = host.open();
    host.held = [];
    const subscribed = channel.subscribeTerminal(SESSION, {
      onFrame: (f) => frames.push(f),
      onEnd: () => undefined,
    });
    await vi.waitFor(() => expect(host.held).toHaveLength(1));
    // The first piece overtakes the reply's verification.
    host.deliver(piece(2, 0, "snapshot", "early"));
    expect(frames).toHaveLength(0);
    host.held.splice(0).forEach((send) => send());
    await subscribed;
    expect(frames.map(text)).toEqual(["early"]);
  });

  it("ends every subscription when the channel closes", async () => {
    const host = await FakeHost.create();
    const { ends } = await following(host);
    host.close();
    expect(ends).toEqual([
      { reason: "connection", detail: "The private connection closed." },
    ]);
  });
});

/**
 * The host closes the channel if more than `MAX_IN_FLIGHT` signed requests
 * queue on it at once. A metadata-heavy page (many cached PR reads) landing
 * on the same channel as a terminal keystroke used to race past the
 * `pending.size` check with four concurrent sends each passing the gate
 * before any of them added to `pending` after the hash `await`, so the
 * host saw more than `MAX_IN_FLIGHT` and closed everyone. These tests
 * exercise the admission slot that now reserves room before any async
 * work.
 */
describe("private channel admission scheduling", () => {
  it("never puts more than MAX_IN_FLIGHT signed requests on the wire at once", async () => {
    const host = await FakeHost.create();
    host.held = [];
    const channel = host.open();
    // Fire one more call than the cap: the surplus must wait for a free
    // slot rather than reach the wire.
    const calls: Promise<unknown>[] = [];
    for (let i = 0; i < MAX_IN_FLIGHT + 1; i++) {
      calls.push(channel.operation("history.get", SESSION, {}));
    }
    await vi.waitFor(() => expect(host.ops()).toHaveLength(MAX_IN_FLIGHT));
    // The surplus call stays unsent while every slot is held.
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(host.ops()).toHaveLength(MAX_IN_FLIGHT);
    // Delivering each held reply frees exactly one slot: the queued call
    // lands on the wire, its own reply gets pushed onto `held`, and so on
    // until every one of the `MAX_IN_FLIGHT + 1` calls has been sent and
    // resolved.
    for (let i = 0; i < MAX_IN_FLIGHT + 1; i++) {
      await vi.waitFor(() => expect(host.held!.length).toBeGreaterThan(0));
      host.held!.shift()!();
    }
    await Promise.all(calls);
    expect(host.ops()).toHaveLength(MAX_IN_FLIGHT + 1);
    // The ops carry distinct ids: the slot gate did not drop or merge any.
    const ids = host.sent.filter((m) => m.t === "op").map((m) => m.id);
    expect(new Set(ids).size).toBe(MAX_IN_FLIGHT + 1);
  });

  it("completes a keystroke input after a metadata-heavy burst instead of refusing it", async () => {
    const host = await FakeHost.create();
    host.held = [];
    const channel = host.open();
    const metadata: Promise<unknown>[] = [];
    for (let i = 0; i < MAX_IN_FLIGHT; i++) {
      metadata.push(channel.operation("history.get", SESSION, {}));
    }
    await vi.waitFor(() => expect(host.ops()).toHaveLength(MAX_IN_FLIGHT));
    // A keystroke op issued while every slot is held does not reject with
    // "Too many requests in flight."; it waits.
    const keystroke = channel.operation("session.input", SESSION, {
      bytes: "a",
    });
    const refused = await Promise.race([
      keystroke.then(
        () => "resolved",
        (e: Error) => `rejected:${e.message}`,
      ),
      new Promise((resolve) => setTimeout(() => resolve("pending"), 30)),
    ]);
    expect(refused).toBe("pending");
    expect(host.ops()).toHaveLength(MAX_IN_FLIGHT);
    // Drain the metadata replies one by one; the keystroke lands on the
    // wire as soon as a slot frees, and completes once its own reply
    // arrives.
    host.held!.shift()!();
    await vi.waitFor(() => expect(host.ops()).toHaveLength(MAX_IN_FLIGHT + 1));
    while (host.held!.length) host.held!.shift()!();
    await Promise.all(metadata);
    await keystroke;
  });

  it("rejects queued waiters when the channel closes without sending them on the wire", async () => {
    const host = await FakeHost.create();
    host.held = [];
    const channel = host.open();
    const inFlight: Promise<unknown>[] = [];
    for (let i = 0; i < MAX_IN_FLIGHT; i++) {
      inFlight.push(channel.operation("history.get", SESSION, {}));
    }
    await vi.waitFor(() => expect(host.ops()).toHaveLength(MAX_IN_FLIGHT));
    // Two surplus callers wait for a slot.
    const waitingA = channel.operation("history.get", SESSION, {});
    const waitingB = channel.operation("history.get", SESSION, {});
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(host.ops()).toHaveLength(MAX_IN_FLIGHT);
    // Closing the channel rejects both the waiters and the in-flight
    // requests; nothing new reaches the wire.
    host.close();
    await expect(waitingA).rejects.toMatchObject({ code: "unavailable" });
    await expect(waitingB).rejects.toMatchObject({ code: "unavailable" });
    for (const call of inFlight) {
      await expect(call).rejects.toMatchObject({ code: "unavailable" });
    }
    expect(host.ops()).toHaveLength(MAX_IN_FLIGHT);
  });
});
