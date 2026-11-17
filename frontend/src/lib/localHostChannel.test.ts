import { beforeEach, afterEach, expect, it, vi } from "vitest";
import { initializeLocalHost, LOCAL_HOST_ID } from "./localHost";
import { hostControl, createInputChannel } from "./hostControl";
import { listSessions, readHistory } from "./privateChannel";
import { openPrivatePty } from "./privatePty";
let calls: string[];
let bodies: unknown[];
class Socket {
  static OPEN = 1;
  static CONNECTING = 0;
  static instances: Socket[] = [];
  readyState = 0;
  sent: unknown[] = [];
  onopen: (() => void) | null = null;
  onclose: ((e: { code: number }) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((e: { data: string }) => void) | null = null;
  constructor(readonly url: URL) {
    Socket.instances.push(this);
  }
  open() {
    this.readyState = 1;
    this.onopen?.();
  }
  send(value: string) {
    this.sent.push(JSON.parse(value));
  }
  close() {
    this.readyState = 3;
  }
  frame(kind: string, b64: string) {
    this.onmessage?.({
      data: JSON.stringify({
        type: "terminal_frame",
        frame: { kind, b64, cols: 80, rows: 24 },
      }),
    });
  }
}
beforeEach(async () => {
  calls = [];
  bodies = [];
  Socket.instances = [];
  localStorage.clear();
  vi.stubGlobal("WebSocket", Socket);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      calls.push(url);
      if (init?.body) bodies.push(JSON.parse(init.body as string));
      const body =
        url === "/api/local"
          ? { local: true, authenticated: true }
          : url.startsWith("/api/local/sessions?")
            ? {
                kind: "sessions",
                sessions: [
                  {
                    id: "s",
                    agent: "codex",
                    cwd: "/work",
                    title: "Local",
                    messages: 1,
                    last_activity: null,
                    running: true,
                  },
                ],
                next: null,
              }
            : url.includes("/history?")
              ? {
                  kind: "history",
                  items: [
                    {
                      seq: 1,
                      uuid: "m",
                      sha256: "hash",
                      size: 1,
                      message: { uuid: "m", text: "local history" },
                    },
                  ],
                  next: null,
                }
              : { status: "executed", detail: null, body: null };
      return new Response(JSON.stringify(body), {
        headers: { "content-type": "application/json" },
      });
    }),
  );
  await initializeLocalHost();
});
afterEach(() => {
  vi.unstubAllGlobals();
});
it("reads and acts through owner APIs without pairing, IndexedDB, OAuth or relay", async () => {
  expect(
    (await hostControl.privateRead(LOCAL_HOST_ID, listSessions))[0].id,
  ).toBe("s");
  expect(
    (
      await hostControl.privateRead(LOCAL_HOST_ID, (c) => readHistory(c, "s"))
    )[0].message.text,
  ).toBe("local history");
  await hostControl.reopenSession("s");
  await hostControl.sendReply("s", "hello");
  const started = await hostControl.startSession("codex");
  expect(bodies).toContainEqual({ session: started.sessionId, agent: "codex" });
  expect(calls.every((url) => url.startsWith("/api/local"))).toBe(true);
});
it("feeds the same private PTY renderer and routes input and resize to the local socket", async () => {
  const onFrame = vi.fn();
  const onReplayComplete = vi.fn();
  const controller = openPrivatePty(LOCAL_HOST_ID, "s", {
    onFrame,
    onReplayComplete,
  });
  const socket = Socket.instances[0];
  socket.open();
  await Promise.resolve();
  socket.frame("snapshot", btoa("hello"));
  socket.frame("output", btoa("world"));
  expect(onFrame).toHaveBeenCalledTimes(2);
  expect(onReplayComplete).toHaveBeenCalled();
  const errors = vi.fn();
  const input = createInputChannel("s", errors, LOCAL_HOST_ID);
  input.send("\u001b");
  await vi.waitFor(() =>
    expect(socket.sent).toContainEqual({ type: "input", b64: btoa("\u001b") }),
  );
  await hostControl.resizeSession("s", 100, 30, LOCAL_HOST_ID);
  expect(socket.sent).toContainEqual({ type: "resize", cols: 100, rows: 30 });
  expect(errors).not.toHaveBeenCalled();
  input.close();
  controller.close();
  await controller.closed;
});

it("adapts owner rename to the signed operation result without central writes", async () => {
  vi.mocked(fetch).mockResolvedValueOnce(
    new Response(JSON.stringify({ title: "Renamed" }), {
      headers: { "content-type": "application/json" },
    }),
  );
  expect(await hostControl.renameSession("s", "Renamed", LOCAL_HOST_ID)).toBe(
    "Renamed",
  );
  expect(fetch).toHaveBeenLastCalledWith(
    "/api/local/sessions/s/rename",
    expect.objectContaining({
      method: "POST",
      body: JSON.stringify({ title: "Renamed" }),
    }),
  );
});

it("sends exact file bytes once and reports only a confirmed receipt as delivered", async () => {
  const { localHostChannel } = await import("./localHostChannel");
  vi.mocked(fetch)
    .mockClear()
    .mockResolvedValueOnce(
      new Response(JSON.stringify({ status: "delivered" }), {
        headers: { "content-type": "application/json" },
      }),
    );
  const progress = vi.fn();
  expect(
    await localHostChannel.sendFiles(
      "s",
      [{ name: "é.txt", bytes: new Uint8Array([0, 255, 128]) }],
      { onProgress: progress },
    ),
  ).toBe("delivered");
  const init = vi.mocked(fetch).mock.calls[0][1]!;
  const body = JSON.parse(init.body as string);
  expect(body.transfer).toMatch(/^[a-f0-9]{32}$/);
  expect(body.content).toEqual({
    kind: "files",
    files: [{ name: "é.txt", b64: "AP+A" }],
  });
  expect(progress.mock.calls).toEqual([
    [0, 3],
    [3, 3],
  ]);
  expect(fetch).toHaveBeenCalledTimes(1);
});
it("does not dispatch cancelled transfers or retry uncertain image delivery", async () => {
  const { localHostChannel } = await import("./localHostChannel");
  vi.mocked(fetch).mockClear();
  const cancelled = AbortSignal.abort();
  expect(
    await localHostChannel.pasteImage("s", "image/png", new Uint8Array([1]), {
      signal: cancelled,
    }),
  ).toBe("cancelled");
  expect(fetch).not.toHaveBeenCalled();
  vi.mocked(fetch).mockRejectedValueOnce(new TypeError("network lost"));
  expect(
    await localHostChannel.pasteImage("s", "image/png", new Uint8Array([1])),
  ).toBe("uncertain");
  expect(fetch).toHaveBeenCalledTimes(1);
});

it("uses owner publication and grant endpoints with canonical audience and exact chosen capabilities", async () => {
  const publication = {
    state: "private",
    rev: 0,
    audience: null,
    published: 0,
    pending: 2,
    error: null,
  };
  vi.mocked(fetch)
    .mockClear()
    .mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          status: "executed",
          detail: JSON.stringify(publication),
          body: null,
        }),
        { headers: { "content-type": "application/json" } },
      ),
    )
    .mockResolvedValueOnce(
      new Response(
        JSON.stringify({ status: "executed", detail: "syncing", body: null }),
        { headers: { "content-type": "application/json" } },
      ),
    )
    .mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          status: "executed",
          detail: "deletion_pending",
          body: null,
        }),
        { headers: { "content-type": "application/json" } },
      ),
    )
    .mockResolvedValue(
      new Response(
        JSON.stringify({ status: "executed", detail: null, body: null }),
        { headers: { "content-type": "application/json" } },
      ),
    );
  expect(await hostControl.publicationStatus(LOCAL_HOST_ID, "s")).toEqual(
    publication,
  );
  expect(
    await hostControl.sharePublication(LOCAL_HOST_ID, "s", {
      visibility: "restricted",
      emails: [" Me@Example.com ", "me@example.com"],
    }),
  ).toBe("syncing");
  expect(await hostControl.unsharePublication(LOCAL_HOST_ID, "s")).toBe(
    "deletion_pending",
  );
  await hostControl.addGrant("s", "key", ["history.read"], LOCAL_HOST_ID);
  vi.mocked(fetch).mockResolvedValueOnce(
    new Response(
      JSON.stringify({ status: "executed", detail: null, body: null }),
      { headers: { "content-type": "application/json" } },
    ),
  );
  await hostControl.removeGrant("s", "key", ["history.read"], LOCAL_HOST_ID);
  const requests = vi.mocked(fetch).mock.calls;
  expect(requests.map((call) => call[0])).toEqual([
    "/api/local/sessions/s/publication",
    "/api/local/sessions/s/publication/share",
    "/api/local/sessions/s/publication/unshare",
    "/api/local/sessions/s/grants/add",
    "/api/local/sessions/s/grants/remove",
  ]);
  expect(JSON.parse(requests[1][1]!.body as string)).toEqual({
    audience: { visibility: "restricted", emails: ["me@example.com"] },
  });
  expect(JSON.parse(requests[3][1]!.body as string)).toEqual({
    recipient: "key",
    caps: ["history.read"],
  });
});

it("normalizes the owner's exact cache-change response for one bounded snapshot restart", async () => {
  const { localHostChannel } = await import("./localHostChannel");
  vi.mocked(fetch).mockResolvedValueOnce(
    new Response(
      JSON.stringify({ error: "PR cache changed; restart from offset 0" }),
      { status: 409, headers: { "content-type": "application/json" } },
    ),
  );
  expect(
    await localHostChannel.operation("pr.cache.list", "s", {
      session: "s",
      offset: 24576,
      sha256: "old",
    }),
  ).toEqual({
    status: "refused",
    detail: "PR cache changed; restart from offset 0",
    body: null,
  });
});

it("reads approved access metadata and Linear pages through owner endpoints", async () => {
  const credential = {
    kid: "A".repeat(43),
    label: "Laptop browser",
    alg: "es256",
    kind: "owner",
    revoked: false,
    grants: [],
  };
  const doc = {
    url: "https://linear.app/team/issue/T-1",
    workspace: "team",
    kind: "issue",
    identifier: "T-1",
  };
  const requests: { url: string; init?: RequestInit }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      requests.push({ url, init });
      return new Response(
        JSON.stringify(
          url === "/api/status"
            ? { credentials: [credential] }
            : init?.method === "DELETE"
              ? { status: "executed", detail: "unlinked", body: null }
              : { kind: "linear", docs: [doc], next: null },
        ),
        { status: 200 },
      );
    }),
  );
  expect(await hostControl.listAccess(LOCAL_HOST_ID)).toEqual([credential]);
  await hostControl.privateRead(LOCAL_HOST_ID, async (channel) => {
    expect(
      (
        await channel.operation("linear.list", "s", {
          session: "s",
          after: null,
          limit: 32,
        })
      ).body,
    ).toEqual({ kind: "linear", docs: [doc], next: null });
    expect(
      (
        await channel.operation("linear.unlink", "s", {
          session: "s",
          url: doc.url,
        })
      ).status,
    ).toBe("executed");
  });
  expect(requests.map((request) => request.url)).toEqual([
    "/api/status",
    "/api/local/sessions/s/linear?limit=32",
    "/api/local/sessions/s/linear",
  ]);
  expect(JSON.parse(requests[2].init?.body as string)).toEqual({
    url: doc.url,
  });
});

it("maps the local human inbox and recipient actions without an account identity", async () => {
  const subject = { kind: "local_owner" };
  const requests: { url: string; body: unknown }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      requests.push({
        url,
        body: init?.body ? JSON.parse(init.body as string) : null,
      });
      return new Response(
        JSON.stringify(
          url.endsWith("attention-recipients")
            ? {
                kind: "attention_recipients",
                recipients: [{ subject, label: "Laptop owner" }],
              }
            : url.includes("/notifications?")
              ? { kind: "notifications", notifications: [], next: null }
              : { status: "executed", detail: null, body: null },
        ),
        { status: 200 },
      );
    }),
  );
  await hostControl.privateRead(LOCAL_HOST_ID, async (channel) => {
    expect(
      (await channel.operation("attention.recipients", "s", { session: "s" }))
        .body?.kind,
    ).toBe("attention_recipients");
    await channel.operation("attention.request", "s", {
      session: "s",
      recipient: subject,
      message: "Review",
      idempotency_key: "uuid",
    });
    await channel.operation("notifications.list", "host", {
      after: null,
      limit: 20,
    });
    await channel.operation("notifications.resolve", "s", {
      session: "s",
      id: "n",
    });
  });
  expect(requests).toEqual([
    { url: "/api/local/sessions/s/attention-recipients", body: null },
    {
      url: "/api/local/sessions/s/attention",
      body: { recipient: subject, message: "Review", idempotency_key: "uuid" },
    },
    { url: "/api/local/notifications?limit=20", body: null },
    { url: "/api/local/sessions/s/notifications/n/resolve", body: {} },
  ]);
});

it("reads bounded communication event pages only from the owner endpoint", async () => {
  const page = {
    kind: "communications",
    peers: [
      {
        session_id: "foreign",
        host_id: "pin",
        title: "Pinned peer",
        agent: null,
        last_seen_at: null,
        connected: null,
        state: "unknown",
        reachable: null,
        readable: false,
      },
    ],
    exchanges: [
      {
        id: "m",
        peer_session_id: "foreign",
        host_id: "pin",
        direction: "sent",
        title: "Review",
        status: "uncertain",
        created_at: null,
      },
    ],
    next: "received:0",
  };
  const urls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      urls.push(url);
      return new Response(JSON.stringify(page), { status: 200 });
    }),
  );
  await hostControl.privateRead(LOCAL_HOST_ID, async (channel) => {
    expect(
      (
        await channel.operation("communications.list", "s", {
          session: "s",
          after: "sent:7",
          limit: 32,
        })
      ).body,
    ).toEqual(page);
  });
  expect(urls).toEqual([
    "/api/local/sessions/s/communications?after=sent%3A7&limit=32",
  ]);
});

it("runs the session-key ceremony through owner-cookie APIs without the relay", async () => {
  const operations: unknown[] = [];
  vi.mocked(fetch).mockImplementation(async (url, init) => {
    expect(url).toBe("/api/local/passkeys");
    expect(init?.credentials).toBe("same-origin");
    const request = JSON.parse(String(init?.body));
    operations.push(request);
    const detail =
      request.op === "enroll_start"
        ? { link: "https://hosted.example/host-link#invite" }
        : request.op === "pending"
          ? [{ cred_id: "credential", kind: "agent" }]
          : request.op === "vouch"
            ? { statement: { p: "payload", s: "signature" }, host_key: "key" }
            : { cred_id: request.cred_id };
    return new Response(
      JSON.stringify({
        status: "executed",
        detail: JSON.stringify(detail),
        body: null,
      }),
      { headers: { "content-type": "application/json" } },
    );
  });
  expect(
    await hostControl.passkeyInvite(LOCAL_HOST_ID, "Colleague", [
      "session.write:s",
    ]),
  ).toBe("https://hosted.example/host-link#invite");
  expect(await hostControl.passkeyPending(LOCAL_HOST_ID)).toEqual([
    { cred_id: "credential", kind: "agent" },
  ]);
  await hostControl.passkeyApprove(LOCAL_HOST_ID, "credential");
  const { passkeyVouch } = await import("./hostControl");
  expect(
    await passkeyVouch("other-host", "challenge", "credential", LOCAL_HOST_ID),
  ).toEqual({ statement: { p: "payload", s: "signature" }, host_key: "key" });
  expect(operations).toEqual([
    {
      op: "enroll_start",
      label: "Colleague",
      kind: "agent",
      grants: ["session.write:s"],
    },
    { op: "pending" },
    { op: "approve", cred_id: "credential" },
    {
      op: "vouch",
      for_host: "other-host",
      challenge: "challenge",
      cred_id: "credential",
    },
  ]);
});
