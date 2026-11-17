import { HostControlError, hostControl } from "./hostControl";
import type { TerminalEnd, TerminalEndReason } from "./hostControl";

/** Callbacks for a single {@link openPrivatePty} attempt. */
export interface LivePtyHandlers {
  /**
   * Called for each PTY frame received. `bytes` is the raw terminal byte
   * sequence (binary, already base64-decoded); forward it to `term.write()`.
   */
  onFrame: (cols: number, rows: number, bytes: Uint8Array) => void;
  /**
   * Called once, right after the channel subscribed this viewer. This is the
   * safe point for reconnect-only host repaint requests: a request fired
   * BEFORE the subscription exists is lost.
   */
  onOpen?: () => void;
  /**
   * Called after the initial replay has been sent in full: the boundary
   * between the replayed snapshot and live PTY output.
   */
  onReplayComplete?: () => void;
}

/** Why an {@link openPrivatePty} attempt ended. */
export type LivePtyEnd = { reason: "eof" | "aborted" };

/** Handle to a private-terminal channel returned by {@link openPrivatePty}. */
export interface LivePtyController {
  /**
   * No-op privately: keystrokes are signed `session.input` operations on the
   * input channel, never bytes on the screen channel. Kept so a caller shaped
   * for a duplex socket keeps type-checking.
   */
  sendInput: (b64: string) => void;
  /** Close the underlying channel. */
  close: () => void;
  /**
   * Resolves `{reason:"eof"}` when the channel closes cleanly (the host ended
   * the stream), or `{reason:"aborted"}` when the passed `AbortSignal` fires.
   * Rejects on a transient drop so the caller's reconnect loop retries.
   */
  closed: Promise<LivePtyEnd>;
}

/** Home, clear screen, clear scrollback: a snapshot replaces what was shown. */
const CLEAR = new Uint8Array([
  0x1b, 0x5b, 0x48, 0x1b, 0x5b, 0x32, 0x4a, 0x1b, 0x5b, 0x33, 0x4a,
]);

/**
 * A followed private terminal stopped for a reason the viewer must act on.
 * `reason: "denied"` needs a visible unlock; nothing here retries it.
 */
export class PrivateTerminalEnded extends HostControlError {
  constructor(
    message: string,
    code: HostControlError["code"],
    readonly reason: TerminalEndReason,
  ) {
    super(message, code);
  }
}

const END_CODE: Record<
  Exclude<TerminalEndReason, "closed" | "gone">,
  HostControlError["code"]
> = {
  // A dropped connection is the only end worth reconnecting by itself.
  connection: "transport",
  expired: "refused",
  denied: "refused",
  protocol: "untrusted",
};

/**
 * The private-channel counterpart of `openLivePty`, with the same controller
 * shape so `LiveTerminal` keeps one renderer: the host's snapshot and updates
 * arrive on the private channel and nothing is read from the server.
 *
 * - Subscribe success is the replay boundary: `onOpen` and `onReplayComplete`
 *   fire as soon as the host admits the subscription, so the viewer can
 *   render an empty terminal instead of waiting on a frame that may be in
 *   flight. The first snapshot that arrives still clears what was shown.
 * - The terminal ending on the host resolves `closed` as `eof`.
 * - A lapsed lease (`expired`) allows one fresh signed subscribe, whose
 *   snapshot repaints the screen; a second lapse stops with an error.
 * - A refusal (`denied`), a broken stream (`protocol`) or a dropped
 *   connection (`connection`, code `transport`) rejects `closed` with a
 *   {@link PrivateTerminalEnded}; so does a subscription that cannot be opened
 *   (with that error's own code). Nothing here retries a refusal.
 * - Keystrokes never go here: `sendInput` is a no-op and input stays on the
 *   signed `session.input` operation.
 */
export function openPrivatePty(
  hostId: string,
  sessionId: string,
  handlers: LivePtyHandlers,
  signal?: AbortSignal,
): LivePtyController {
  let settled = false;
  let resolveClosed!: (end: LivePtyEnd) => void;
  let rejectClosed!: (error: unknown) => void;
  const closed = new Promise<LivePtyEnd>((resolve, reject) => {
    resolveClosed = resolve;
    rejectClosed = reject;
  });
  const settle = (end: LivePtyEnd | Error) => {
    if (settled) return;
    settled = true;
    signal?.removeEventListener("abort", onAbort);
    if (end instanceof Error) rejectClosed(end);
    else resolveClosed(end);
  };

  let subscription: { close(): void } | null = null;
  let opened = false;
  let signaled = false;
  let resubscribed = false;
  const close = () => {
    subscription?.close();
    subscription = null;
  };
  const onAbort = () => {
    close();
    settle({ reason: "aborted" });
  };
  if (signal?.aborted) onAbort();
  else signal?.addEventListener("abort", onAbort, { once: true });

  const subscribe = () => {
    // Each subscription's frames and end are only its own: a late end from a
    // replaced one must not stop the fresh one.
    let current = true;
    hostControl
      .subscribeTerminal(hostId, sessionId, {
        onFrame({ kind, cols, rows, data }) {
          if (settled || !current) return;
          if (kind === "snapshot") {
            const bytes = new Uint8Array(CLEAR.length + data.length);
            bytes.set(CLEAR);
            bytes.set(data, CLEAR.length);
            opened = true;
            handlers.onFrame(cols, rows, bytes);
            if (!signaled) {
              signaled = true;
              handlers.onOpen?.();
              handlers.onReplayComplete?.();
            }
            return;
          }
          // An update before any snapshot has nothing to apply to.
          if (opened) handlers.onFrame(cols, rows, data);
        },
        onEnd(end: TerminalEnd) {
          if (settled || !current) return;
          current = false;
          subscription = null;
          if (end.reason === "closed" || end.reason === "gone") {
            settle({ reason: "eof" });
          } else if (end.reason === "expired" && !resubscribed) {
            resubscribed = true;
            subscribe();
          } else {
            settle(
              new PrivateTerminalEnded(
                end.detail,
                END_CODE[end.reason],
                end.reason,
              ),
            );
          }
        },
      })
      .then(
        (sub) => {
          if (settled || !current) {
            sub.close();
            return;
          }
          subscription = sub;
          // Tell the viewer the terminal is live as soon as the host admits
          // the subscription, so the UI can stop showing "Loading terminal…"
          // while waiting for the first snapshot. A snapshot that arrives
          // later still repaints the screen via its CLEAR prefix.
          if (!signaled) {
            signaled = true;
            handlers.onOpen?.();
            handlers.onReplayComplete?.();
          }
        },
        (error: unknown) => {
          current = false;
          settle(error instanceof Error ? error : new Error(String(error)));
        },
      );
  };
  if (!settled) subscribe();

  return { sendInput: () => {}, close, closed };
}
