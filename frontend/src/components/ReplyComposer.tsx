import * as React from "react";
import { Button } from "@/components/ui/button";
import { useToast } from "@/components/ui/toast";
import { ApiError } from "@/lib/api";
import { hostControl, HostControlError } from "@/lib/hostControl";

/**
 * Reply composer for a session followed from its own host (`hostId` is always
 * set). Injects the typed text as a signed `session.input` operation over the
 * private channel to the host, which checks it against its local grants.
 *
 * Rendered ONLY when the session is interactive AND the viewer may write.
 * Otherwise it renders nothing — no composer, no hint.
 *
 * No optimistic echo: the injected reply surfaces naturally in the transcript
 * via the live tail once the agent processes it.
 */
export function ReplyComposer({
  id,
  mode,
  viewerCanWrite,
  onUndelivered,
  hostId,
}: {
  id: string;
  hostId?: string;
  mode: "view" | "interactive" | undefined;
  viewerCanWrite: boolean;
  /** Called when a reply comes back undelivered (no live CLI attached). */
  onUndelivered?: () => void;
}) {
  const { toast } = useToast();
  const [text, setText] = React.useState("");
  const [sending, setSending] = React.useState(false);

  const visible = mode === "interactive" && viewerCanWrite;
  if (!visible) return null;

  async function send() {
    const value = text.trim();
    if (!value || sending) return;
    setSending(true);
    try {
      if (hostId) await hostControl.sendReply(id, value, hostId);
      else await hostControl.sendReply(id, value);
      setText("");
    } catch (err) {
      onUndelivered?.();
      const message =
        err instanceof HostControlError
          ? err.message
          : err instanceof ApiError
            ? err.status === 403
              ? "You can only reply to your own sessions."
              : err.status === 404
                ? "Reply-back is not available for this session."
                : err.message
            : ((err as Error)?.message ?? "Could not send reply.");
      toast(message, "error");
    } finally {
      setSending(false);
    }
  }

  return (
    <form
      className="flex items-end gap-2 rounded-xl border border-border bg-card p-3"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <textarea
        aria-label="Reply"
        placeholder="Reply to the running session…"
        rows={2}
        value={text}
        disabled={sending}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            void send();
          }
        }}
        className="flex-1 resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
      />
      <Button type="submit" disabled={sending || !text.trim()}>
        {sending ? "Sending…" : "Send"}
      </Button>
    </form>
  );
}
