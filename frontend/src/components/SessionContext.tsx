import { useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  useNotificationAction,
  useNotifications,
  notificationSender,
  type NotificationItem,
} from "@/lib/useNotifications";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";

export function sessionContext(
  notifications: NotificationItem[],
  id: string,
  hostId?: string,
) {
  const related = notifications.filter(
    (item) => item.session_id === id && item.hostId === hostId,
  );
  const attention = related.filter(
    (item) => item.kind === "attention" && !item.resolved_at,
  );
  const share = related
    .filter((item) => item.kind === "share" && item.message.trim())
    .sort((a, b) => b.created_at.localeCompare(a.created_at))[0];
  return { attention, share };
}

export function ContextIcon({
  message,
  attention = false,
  onActivate,
}: {
  message: string;
  attention?: boolean;
  onActivate?: () => void;
}) {
  const [position, setPosition] = useState<{
    top: number;
    left: number;
  } | null>(null);
  const [pinned, setPinned] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const closing = useRef<ReturnType<typeof setTimeout> | null>(null);
  const id = useId();
  function show() {
    if (closing.current) clearTimeout(closing.current);
    const bounds = button.current?.getBoundingClientRect();
    if (bounds)
      setPosition({
        top: Math.min(bounds.bottom, window.innerHeight - 180),
        left: Math.max(8, Math.min(bounds.left, window.innerWidth - 300)),
      });
  }
  function hideSoon() {
    if (pinned) return;
    closing.current = setTimeout(() => setPosition(null), 100);
  }
  useEffect(() => {
    if (!position) return;
    function keydown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        setPosition(null);
        setPinned(false);
      }
    }
    const hide = (event: Event) => {
      if (
        event.target instanceof Element &&
        event.target.closest('[role="tooltip"]')
      )
        return;
      setPosition(null);
      setPinned(false);
    };
    window.addEventListener("keydown", keydown);
    window.addEventListener("resize", hide);
    window.addEventListener("scroll", hide, true);
    return () => {
      window.removeEventListener("keydown", keydown);
      window.removeEventListener("resize", hide);
      window.removeEventListener("scroll", hide, true);
    };
  }, [position]);
  useEffect(
    () => () => {
      if (closing.current) clearTimeout(closing.current);
    },
    [],
  );
  return (
    <>
      <button
        ref={button}
        type="button"
        aria-label={attention ? "Attention request" : "Share reason"}
        aria-describedby={position ? id : undefined}
        aria-expanded={Boolean(position)}
        onMouseEnter={show}
        onMouseLeave={hideSoon}
        onFocus={show}
        onBlur={() => {
          setPosition(null);
          setPinned(false);
        }}
        onClick={() => {
          if (onActivate) {
            setPosition(null);
            onActivate();
          } else if (pinned) {
            setPosition(null);
            setPinned(false);
          } else {
            setPinned(true);
            show();
          }
        }}
        className={`shrink-0 rounded px-1 text-sm focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring ${attention ? "text-red-600 dark:text-red-400" : "text-muted-foreground"}`}
      >
        {attention ? "!" : "ⓘ"}
      </button>
      {position &&
        createPortal(
          <span
            id={id}
            role="tooltip"
            onMouseEnter={show}
            onMouseLeave={hideSoon}
            style={position}
            className="fixed z-90 max-h-40 w-72 max-w-[calc(100vw-1rem)] overflow-auto whitespace-pre-wrap break-words rounded-md border border-border bg-popover p-3 text-left text-sm font-normal text-popover-foreground shadow-lg"
          >
            {message}
          </span>,
          document.body,
        )}
    </>
  );
}

export function AttentionIndicator({
  requests,
}: {
  requests: NotificationItem[];
}) {
  const [open, setOpen] = useState(false);
  const action = useNotificationAction();
  return (
    <>
      {requests.length > 0 && (
        <ContextIcon
          attention
          message={requests
            .map((item) => `${notificationSender(item)}: ${item.message}`)
            .join("\n\n")}
          onActivate={() => setOpen(true)}
        />
      )}
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Attention requests</DialogTitle>
            <DialogDescription>
              Mark each request resolved when you have addressed it.
            </DialogDescription>
          </DialogHeader>
          {requests.length === 0 && (
            <p className="text-sm text-muted-foreground">
              All requests resolved.
            </p>
          )}
          <ul className="max-h-[60vh] space-y-3 overflow-auto">
            {requests.map((item) => (
              <li
                key={item.id}
                className="space-y-2 rounded-lg border border-border p-3"
              >
                <p className="text-sm font-medium">
                  {notificationSender(item)}
                </p>
                <p className="whitespace-pre-wrap break-words text-sm">
                  {item.message}
                </p>
                <Button
                  size="sm"
                  disabled={action.isPending}
                  onClick={() =>
                    action.mutate({
                      id: item.id,
                      hostId: item.hostId,
                      session_id: item.session_id,
                      resolve: true,
                    })
                  }
                >
                  Mark resolved
                </Button>
              </li>
            ))}
          </ul>
          {action.error && (
            <p role="alert" className="text-sm text-destructive">
              {action.error.message}
            </p>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

export function SessionContext({
  sessionId,
  hostId,
}: {
  sessionId: string;
  hostId?: string;
}) {
  const { notifications } = useNotifications();
  const { share, attention } = sessionContext(notifications, sessionId, hostId);
  return (
    <span className="inline-flex items-center gap-1">
      {share && (
        <ContextIcon
          message={`${notificationSender(share)}: ${share.message}`}
        />
      )}
      <AttentionIndicator requests={attention} />
    </span>
  );
}
