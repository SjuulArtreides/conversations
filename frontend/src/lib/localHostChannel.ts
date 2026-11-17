import { localRequest } from "./localHost";
import {
  checkBody,
  type ChannelResult,
  type TransferOptions,
  type TransferStatus,
  type PrivateChannel,
  type TerminalHandlers,
  type TerminalSubscription,
} from "./privateChannel";

const sockets = new Map<string, Set<WebSocket>>();
const sizes = new Map<string, { cols: number; rows: number }>();
const done = (): ChannelResult => ({
  status: "executed",
  detail: null,
  body: null,
});
const unsupported = (feature: string): never => {
  throw new Error(
    `${feature} is not yet available through the local host connection.`,
  );
};
const path = (id: string) => `/api/local/sessions/${encodeURIComponent(id)}`;
async function read(url: string): Promise<ChannelResult> {
  return {
    status: "executed",
    detail: null,
    body: checkBody(await localRequest<unknown>(url)),
  };
}
function query(
  values: Record<string, string | number | boolean | null | undefined>,
) {
  const result = new URLSearchParams();
  for (const [key, value] of Object.entries(values))
    if (value !== null && value !== undefined) result.set(key, String(value));
  return result.toString();
}
async function mutate(url: string, body: unknown): Promise<ChannelResult> {
  const result = await localRequest<ChannelResult>(url, {
    method: "POST",
    body: JSON.stringify(body),
  });
  return actionResult(result);
}
function actionResult(result: ChannelResult): ChannelResult {
  if (
    !["executed", "replayed", "refused", "failed", "uncertain"].includes(
      result.status,
    ) ||
    (result.detail !== null && typeof result.detail !== "string")
  )
    throw new Error("Malformed local action result.");
  return { ...result, body: checkBody(result.body) };
}
function send(session: string, command: unknown) {
  const socket = [...(sockets.get(session) ?? [])].find(
    (value) => value.readyState === WebSocket.OPEN,
  );
  if (!socket)
    throw new Error(
      "The local terminal is not connected. Reconnect before typing.",
    );
  socket.send(JSON.stringify(command));
}

function subscribe(
  session: string,
  handlers: TerminalHandlers,
): Promise<TerminalSubscription> {
  const url = new URL(
    `/api/terminal/${encodeURIComponent(session)}`,
    window.location.href,
  );
  url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(url);
  let stopped = false;
  let opened = false;
  const set = sockets.get(session) ?? new Set<WebSocket>();
  set.add(ws);
  sockets.set(session, set);
  const forget = () => {
    set.delete(ws);
    if (!set.size) {
      sockets.delete(session);
      sizes.delete(session);
    }
  };
  return new Promise((resolve, reject) => {
    const end = (
      reason: "connection" | "protocol" | "denied" | "gone",
      detail: string,
    ) => {
      if (stopped) return;
      stopped = true;
      forget();
      ws.close();
      if (!opened) reject(new Error(detail));
      else handlers.onEnd({ reason, detail });
    };
    ws.onopen = () => {
      opened = true;
      resolve({
        sub: 0,
        close() {
          if (stopped) return;
          stopped = true;
          forget();
          ws.close();
          queueMicrotask(() =>
            handlers.onEnd({
              reason: "closed",
              detail: "The viewer closed this terminal.",
            }),
          );
        },
      });
    };
    ws.onmessage = (event) => {
      if (stopped) return;
      try {
        const message = JSON.parse(String(event.data));
        if (message.type === "error") {
          end(
            "denied",
            String(message.message ?? "Local terminal refused access."),
          );
          return;
        }
        const frame = message.frame;
        if (
          message.type !== "terminal_frame" ||
          !frame ||
          !["snapshot", "output"].includes(frame.kind) ||
          !Number.isInteger(frame.cols) ||
          frame.cols <= 0 ||
          !Number.isInteger(frame.rows) ||
          frame.rows <= 0 ||
          typeof frame.b64 !== "string"
        )
          throw new Error("Malformed local terminal frame.");
        const data = Uint8Array.from(atob(frame.b64), (character) =>
          character.charCodeAt(0),
        );
        sizes.set(session, { cols: frame.cols, rows: frame.rows });
        handlers.onFrame({
          kind: frame.kind === "snapshot" ? "snapshot" : "update",
          cols: frame.cols,
          rows: frame.rows,
          data,
        });
      } catch {
        end("protocol", "The local terminal sent an unreadable frame.");
      }
    };
    ws.onerror = () =>
      end("connection", "The local terminal connection failed.");
    ws.onclose = (event) =>
      end(
        event.code === 1000 ? "gone" : "connection",
        "The local terminal connection closed.",
      );
  });
}

function standardBase64(bytes: Uint8Array): string {
  let text = "";
  for (let offset = 0; offset < bytes.length; offset += 16_384)
    text += String.fromCharCode(...bytes.subarray(offset, offset + 16_384));
  return btoa(text);
}
async function transfer(
  session: string,
  content: unknown,
  total: number,
  opts: TransferOptions = {},
): Promise<TransferStatus> {
  if (opts.signal?.aborted) return "cancelled";
  opts.onProgress?.(0, total);
  const id = Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  try {
    const reply = await localRequest<{ status: string }>(
      `${path(session)}/transfer`,
      {
        method: "POST",
        body: JSON.stringify({ transfer: id, content }),
        signal: opts.signal,
      },
    );
    if (!["delivered", "failed", "refused", "uncertain"].includes(reply.status))
      return "uncertain";
    if (reply.status === "delivered") opts.onProgress?.(total, total);
    return reply.status as TransferStatus;
  } catch {
    // Once dispatched, abort/network failure cannot prove whether paths were pasted.
    return "uncertain";
  }
}

/** Same channel interface; local owner cookie replaces remote signed delegation. */
export const localHostChannel: PrivateChannel = {
  peer: "local-owner",
  open: true,
  close() {
    for (const set of sockets.values())
      for (const socket of set) socket.close();
    sockets.clear();
    sizes.clear();
  },
  selectedPair: async () => null,
  historyGet: (session, after = 0, limit = 32, digest = false) =>
    read(`${path(session)}/history?${query({ after, limit, digest })}`),
  historyFragment: (session, seq, offset) =>
    read(`${path(session)}/fragment?${query({ seq, offset })}`),
  historySearch: (session, text, limit = 50) =>
    read(`${path(session)}/search?${query({ query: text, limit })}`),
  sessionsList: (after = null, limit = 50, running_only = false) =>
    read(
      `/api/local/sessions?${query({ after, limit, readiness: true, ...(running_only ? { running_only } : {}) })}`,
    ),
  settingsGet: () => read("/api/local/settings"),
  subscribeTerminal: subscribe,
  sendSigned: async () => unsupported("Signed remote retries"),
  sendFiles: async (session, files, opts) => {
    const total = files.reduce((sum, file) => sum + file.bytes.length, 0);
    if (!files.length || files.length > 16 || total > 9 * 1024 * 1024)
      throw new Error("Choose 1–16 files totalling at most 9 MiB.");
    return transfer(
      session,
      {
        kind: "files",
        files: files.map((file) => ({
          name: file.name,
          b64: standardBase64(file.bytes),
        })),
      },
      total,
      opts,
    );
  },
  pasteImage: async (session, mime, bytes, opts) => {
    if (!bytes.length || bytes.length > 9 * 1024 * 1024)
      throw new Error("Images must be between 1 byte and 9 MiB.");
    return transfer(
      session,
      { kind: "image", mime, b64: standardBase64(bytes) },
      bytes.length,
      opts,
    );
  },
  async operation(op, session, args = {}) {
    switch (op) {
      case "migration.export":
        return read(
          `${path(session)}/migration?${query({ transfer: args.transfer as string, offset: args.offset as number })}`,
        );
      case "migration.import":
        return mutate("/api/local/migration", args);
      case "passkey.enroll_start":
      case "passkey.pending":
      case "passkey.approve":
      case "passkey.vouch":
        return mutate("/api/local/passkeys", {
          op: op.slice("passkey.".length),
          ...args,
        });
      case "session.start":
        return mutate("/api/local/start", { session, ...args });
      case "session.reopen":
        return mutate(`${path(session)}/reopen`, {});
      case "session.rename": {
        const result = await localRequest<{ title: string }>(
          `${path(session)}/rename`,
          { method: "POST", body: JSON.stringify({ title: args.title }) },
        );
        if (typeof result.title !== "string")
          throw new Error("The host did not return the renamed title.");
        return { ...done(), detail: result.title };
      }
      case "session.end":
        return mutate(`${path(session)}/end`, {});
      case "session.reply":
        return mutate(`${path(session)}/reply`, { text: args.text });
      case "communications.list":
        return read(
          `${path(session)}/communications?${query({ after: args.after as string | undefined, limit: args.limit as number | undefined })}`,
        );
      case "attention.recipients":
        return read(`${path(session)}/attention-recipients`);
      case "attention.request":
        return mutate(`${path(session)}/attention`, {
          recipient: args.recipient,
          message: args.message,
          idempotency_key: args.idempotency_key,
        });
      case "notifications.list":
        return read(
          `/api/local/notifications?${query({ after: args.after as number | undefined, limit: args.limit as number | undefined })}`,
        );
      case "notifications.read":
      case "notifications.resolve":
        return mutate(
          `${path(session)}/notifications/${encodeURIComponent(String(args.id))}/${op === "notifications.read" ? "read" : "resolve"}`,
          {},
        );
      case "linear.list":
        return read(
          `${path(session)}/linear?${query({ after: args.after as string | undefined, limit: args.limit as number | undefined })}`,
        );
      case "linear.unlink":
        return localRequest<ChannelResult>(`${path(session)}/linear`, {
          method: "DELETE",
          body: JSON.stringify({ url: args.url }),
        });
      case "pr.cache.list": {
        try {
          return await read(
            `${path(session)}/prs?${query({ offset: args.offset as number | undefined, sha256: args.sha256 as string | undefined })}`,
          );
        } catch (error) {
          if (
            error instanceof Error &&
            error.message === "PR cache changed; restart from offset 0"
          )
            return { status: "refused", detail: error.message, body: null };
          throw error;
        }
      }
      case "pr.unlink":
        return mutate(`${path(session)}/prs/unlink`, {
          owner: args.owner,
          repo: args.repo,
          number: args.number,
        });
      case "pr.sync":
        return mutate(`${path(session)}/prs/sync`, {
          owner: args.owner,
          repo: args.repo,
          number: args.number,
        });
      case "pr.comment":
        return mutate(`${path(session)}/prs/comment`, {
          owner: args.owner,
          repo: args.repo,
          number: args.number,
          body: args.body,
          ...(args.in_reply_to != null
            ? { in_reply_to: args.in_reply_to }
            : {}),
        });
      case "access.list": {
        const result = await localRequest<{ credentials: unknown[] }>(
          "/api/status",
        );
        return {
          ...done(),
          body: checkBody({ kind: "access", credentials: result.credentials }),
        };
      }
      case "projects.list": {
        const result = await localRequest<{ projects: unknown[] }>(
          "/api/projects",
        );
        return {
          ...done(),
          body: checkBody({ kind: "projects", projects: result.projects }),
        };
      }
      case "projects.create": {
        const result = await localRequest<unknown>("/api/projects", {
          method: "POST",
          body: JSON.stringify({
            name: args.name,
            description: args.description,
          }),
        });
        return { ...done(), detail: JSON.stringify(result) };
      }
      case "session.project.get": {
        const result = await localRequest<{ project_id: string | null }>(
          `/api/sessions/${encodeURIComponent(session)}/project`,
        );
        return {
          ...done(),
          body: checkBody({
            kind: "project_membership",
            project_id: result.project_id,
          }),
        };
      }
      case "session.project.set": {
        const result = await localRequest<unknown>(
          `/api/sessions/${encodeURIComponent(session)}/project`,
          {
            method: "PUT",
            body: JSON.stringify({ project_id: args.project_id }),
          },
        );
        return { ...done(), detail: JSON.stringify(result) };
      }
      case "publication.status":
        return actionResult(
          await localRequest<ChannelResult>(`${path(session)}/publication`),
        );
      case "publication.share":
        return mutate(`${path(session)}/publication/share`, {
          audience: args.audience,
        });
      case "publication.unshare":
        return mutate(`${path(session)}/publication/unshare`, {});
      case "grant.add":
      case "grant.remove":
        return mutate(
          `${path(session)}/grants/${op === "grant.add" ? "add" : "remove"}`,
          { recipient: args.recipient, caps: args.caps },
        );
      case "settings.update":
        return mutate("/api/local/settings", { launch: args.launch });
      case "session.input":
        send(session, { type: "input", b64: args.b64 });
        return done();
      case "session.resize":
        send(session, { type: "resize", cols: args.cols, rows: args.rows });
        return done();
      case "session.repaint":
        send(session, { type: "repaint" });
        return done();
      default:
        return unsupported(op);
    }
  },
};
