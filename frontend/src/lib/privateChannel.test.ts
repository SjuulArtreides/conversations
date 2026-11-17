import { describe, expect, it } from "vitest";
import { toBase64Url } from "./hostAuth";
import {
  listSessions,
  readHistory,
  refreshHistory,
  type ChannelResult,
  type HistoryItem,
  type PrivateChannel,
  type SessionEntry,
} from "./privateChannel";

const SESSION = "11111111-2222-3333-4444-555555555555";
const INLINE = 16 * 1024;
const PIECE = 24 * 1024;

async function sha(bytes: Uint8Array): Promise<string> {
  return toBase64Url(
    new Uint8Array(
      await crypto.subtle.digest("SHA-256", bytes as unknown as BufferSource),
    ),
  );
}

/** A host's local history, answered the way the host answers on the channel. */
class FakeHost {
  rows: { seq: number; uuid: string; json: string }[] = [];
  calls: string[] = [];
  sessions: SessionEntry[] = [];
  /** Rewrite a message between the page and its fragments, once. */
  rewriteDuringFragment: (() => void) | null = null;

  put(uuid: string, message: Record<string, unknown>) {
    const json = JSON.stringify({ uuid, ...message });
    const row = this.rows.find((r) => r.uuid === uuid);
    if (row) row.json = json;
    else this.rows.push({ seq: this.rows.length + 1, uuid, json });
  }

  async item(
    row: { seq: number; uuid: string; json: string },
    digest: boolean,
  ) {
    const bytes = new TextEncoder().encode(row.json);
    return {
      seq: row.seq,
      uuid: row.uuid,
      size: bytes.length,
      sha256: await sha(bytes),
      message:
        !digest && bytes.length <= INLINE
          ? (JSON.parse(row.json) as Record<string, unknown>)
          : null,
    } satisfies HistoryItem;
  }

  channel(): PrivateChannel {
    const ok = (body: ChannelResult["body"]): ChannelResult => ({
      status: "executed",
      detail: null,
      body,
    });
    return {
      peer: "p",
      open: true,
      close: () => undefined,
      operation: () => Promise.reject(new Error("unused")),
      sendSigned: () => Promise.reject(new Error("unused")),
      selectedPair: () => Promise.resolve(null),
      historySearch: () => Promise.reject(new Error("unused")),
      settingsGet: () => Promise.reject(new Error("unused")),
      sendFiles: () => Promise.reject(new Error("unused")),
      pasteImage: () => Promise.reject(new Error("unused")),
      subscribeTerminal: () => Promise.reject(new Error("unused")),
      historyGet: async (_s, after = 0, limit = 32, digest = false) => {
        this.calls.push(`get:${after}:${limit}:${digest}`);
        const page = this.rows.filter((r) => r.seq > after).slice(0, limit);
        const items = await Promise.all(page.map((r) => this.item(r, digest)));
        const last = page.at(-1);
        const more = last && this.rows.some((r) => r.seq > last.seq);
        return ok({ kind: "history", items, next: more ? last.seq : null });
      },
      historyFragment: async (_s, seq, offset) => {
        this.calls.push(`fragment:${seq}:${offset}`);
        if (offset > 0 && this.rewriteDuringFragment) {
          this.rewriteDuringFragment();
          this.rewriteDuringFragment = null;
        }
        const row = this.rows.find((r) => r.seq === seq)!;
        const bytes = new TextEncoder().encode(row.json);
        const end = Math.min(bytes.length, offset + PIECE);
        return ok({
          kind: "fragment",
          seq,
          offset,
          data: toBase64Url(bytes.slice(offset, end)),
          size: bytes.length,
          sha256: await sha(bytes),
          next: end < bytes.length ? end : null,
        });
      },
      sessionsList: async (after = null) => {
        const start = after === null ? 0 : Number(after);
        const page = this.sessions.slice(start, start + 2);
        const next =
          start + 2 < this.sessions.length ? String(start + 2) : null;
        return ok({ kind: "sessions", sessions: page, next });
      },
    };
  }
}

const TOOL_USE = {
  role: "assistant",
  blocks: [
    { type: "text", text: "Running the tests." },
    {
      type: "tool_use",
      id: "toolu_1",
      name: "Bash",
      input: { command: "cargo test" },
    },
  ],
};

describe("private history", () => {
  it("reads every canonical message complete, large ones from hash-checked pieces", async () => {
    const host = new FakeHost();
    host.put("u1", TOOL_USE);
    const code = "fn main() {}\n".repeat(5_000);
    host.put("u2", {
      role: "assistant",
      blocks: [{ type: "text", text: code }],
    });
    const messages = await readHistory(host.channel(), SESSION);
    expect(messages.map((m) => m.seq)).toEqual([1, 2]);
    expect(messages[0].message.blocks).toEqual(TOOL_USE.blocks);
    expect((messages[1].message.blocks as { text: string }[])[0].text).toBe(
      code,
    );
    expect(host.calls.filter((c) => c.startsWith("fragment:2:")).length).toBe(
      3,
    );
  });

  it("never joins pieces of a message that changed while it was read", async () => {
    const host = new FakeHost();
    const before = "a".repeat(60_000);
    const after = "b".repeat(70_000);
    host.put("u1", {
      role: "assistant",
      blocks: [{ type: "text", text: before }],
    });
    host.rewriteDuringFragment = () =>
      host.put("u1", {
        role: "assistant",
        blocks: [{ type: "text", text: after }],
      });
    const [message] = await readHistory(host.channel(), SESSION);
    expect((message.message.blocks as { text: string }[])[0].text).toBe(after);
  });

  it("finds a rewrite of an earlier message that kept its position", async () => {
    const host = new FakeHost();
    host.put("u1", TOOL_USE);
    host.put("u2", { role: "user", blocks: [{ type: "text", text: "next" }] });
    const channel = host.channel();
    const first = await readHistory(channel, SESSION);
    // The tool result arrives in the SAME message: its seq does not move.
    const finished = {
      ...TOOL_USE,
      blocks: [
        ...TOOL_USE.blocks,
        { type: "tool_result", tool_use_id: "toolu_1", content: "all green" },
      ],
    };
    host.put("u1", finished);
    host.put("u3", {
      role: "assistant",
      blocks: [{ type: "text", text: "done" }],
    });
    const { messages, changed } = await refreshHistory(channel, SESSION, first);
    expect(changed).toBe(2);
    expect(messages.map((m) => m.uuid)).toEqual(["u1", "u2", "u3"]);
    expect(messages[0].message.blocks).toEqual(finished.blocks);
    expect(messages[1]).toBe(first[1]);
  });
});

describe("private session list", () => {
  it("follows the host's cursor across pages", async () => {
    const host = new FakeHost();
    host.sessions = [1, 2, 3].map((n) => ({
      id: `s${n}`,
      agent: "claude",
      cwd: "/work",
      title: `t${n}`,
      messages: n,
      last_activity: null,
      running: false,
    }));
    const list = await listSessions(host.channel());
    expect(list.map((s) => s.id)).toEqual(["s1", "s2", "s3"]);
  });
});

describe("complete private session enumeration", () => {
  const entry = (id: string): SessionEntry => ({
    id,
    agent: "codex",
    cwd: "/work",
    title: id,
    messages: 1,
    last_activity: null,
    running: false,
  });
  it("loads all 2449 imported sessions rather than stopping after 20 pages", async () => {
    const host = new FakeHost();
    host.sessions = Array.from({ length: 2449 }, (_, i) =>
      entry(`session-${i}`),
    );
    expect(await listSessions(host.channel())).toEqual(host.sessions);
  });
  it("retains an explicitly requested page limit", async () => {
    const host = new FakeHost();
    host.sessions = Array.from({ length: 10 }, (_, i) => entry(`session-${i}`));
    expect(await listSessions(host.channel(), { maxPages: 2 })).toEqual(
      host.sessions.slice(0, 4),
    );
  });
  it.each([
    ["stuck", "stuck"],
    ["a", "b", "a"],
  ])("rejects cursor cycles: %j", async (...cursors) => {
    let requests = 0;
    const channel = {
      sessionsList: async () => ({
        status: "executed",
        detail: null,
        body: {
          kind: "sessions",
          sessions: [entry("s")],
          next: cursors[requests++ % cursors.length],
        },
      }),
    } as unknown as PrivateChannel;
    await expect(listSessions(channel)).rejects.toThrow(
      /pagination did not advance/i,
    );
    expect(requests).toBe(cursors.length);
  });
});

describe("progressive private history", () => {
  it("publishes complete pages before a later page arrives, and reads beyond 5000 messages", async () => {
    const host = new FakeHost();
    for (let n = 1; n <= 5001; n++)
      host.put(`m${n}`, { role: "user", text: `message ${n}` });
    const channel = host.channel();
    const get = channel.historyGet;
    let release!: () => void;
    const delayed = new Promise<void>((resolve) => {
      release = resolve;
    });
    channel.historyGet = async (...args) => {
      if (args[1] === 32) await delayed;
      return get(...args);
    };
    const pages: number[][] = [];
    const reading = readHistory(channel, SESSION, {
      onPage: (page) => pages.push(page.map((m) => m.seq)),
    });
    // Allow the first page's asynchronous hashes to finish without releasing page two.
    while (!pages.length)
      await new Promise((resolve) => setTimeout(resolve, 1));
    expect(pages[0]).toEqual(Array.from({ length: 32 }, (_, i) => i + 1));
    expect(pages).toHaveLength(1);
    release();
    const messages = await reading;
    expect(messages).toHaveLength(5001);
    expect(messages.at(-1)?.seq).toBe(5001);
    expect(pages.flat()).toEqual(messages.map((m) => m.seq));
  });

  it("cancels between fragments without publishing an incomplete page", async () => {
    const host = new FakeHost();
    host.put("large", { text: "x".repeat(60000) });
    const channel = host.channel();
    const fragment = channel.historyFragment;
    const abort = new AbortController();
    channel.historyFragment = async (...args) => {
      const result = await fragment(...args);
      abort.abort();
      return result;
    };
    const pages: unknown[] = [];
    await expect(
      readHistory(channel, SESSION, {
        signal: abort.signal,
        onPage: (page) => pages.push(page),
      }),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(pages).toHaveLength(0);
    expect(host.calls.filter((c) => c.startsWith("fragment:"))).toHaveLength(1);
  });

  it("rejects a non-advancing history cursor rather than looping or returning incomplete history", async () => {
    const channel = new FakeHost().channel();
    channel.historyGet = async () => ({
      status: "executed",
      detail: null,
      body: { kind: "history", items: [], next: 0 },
    });
    await expect(readHistory(channel, SESSION)).rejects.toThrow(
      /pagination did not advance/,
    );
  });
});
