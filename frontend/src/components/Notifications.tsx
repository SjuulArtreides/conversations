import { useState } from "react";
import { Link } from "react-router-dom";
import {
  useNotifications,
  notificationSender,
  useNotificationAction,
} from "@/lib/useNotifications";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";

export function Notifications() {
  const { notifications, isPending, isError, errors, refetch, canRequest } =
    useNotifications();
  const action = useNotificationAction();
  const [open, setOpen] = useState(false);
  if (!canRequest) return null;
  const unread = notifications.filter((item) => !item.read_at).length;
  return (
    <>
      <Button
        variant="outline"
        size="sm"
        onClick={() => setOpen(true)}
        aria-label={`Notifications${unread ? ` (${unread} unread)` : ""}`}
        className="shrink-0 gap-1.5"
      >
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          className="h-4 w-4"
          aria-hidden
        >
          <path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9M10 21h4" />
        </svg>
        {unread > 0 && (
          <span className="rounded-full bg-red-500 px-1.5 text-xs text-white">
            {unread}
          </span>
        )}
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Notifications</DialogTitle>
            <DialogDescription>
              Sharing context and requests from your colleagues.
            </DialogDescription>
          </DialogHeader>
          {isPending && <p>Loading notifications…</p>}
          {isError && (
            <p role="alert">
              {errors.join(" ")}{" "}
              <button className="underline" onClick={() => void refetch()}>
                Retry
              </button>
            </p>
          )}
          {!isPending && !isError && notifications.length === 0 && (
            <p className="text-sm text-muted-foreground">
              No notifications yet.
            </p>
          )}
          <ul className="max-h-[65vh] space-y-3 overflow-auto">
            {notifications.map((item) => (
              <li
                key={`${item.hostId ?? "central"}:${item.id}`}
                className={`space-y-2 rounded-lg border p-3 ${item.kind === "attention" && !item.resolved_at ? "border-red-500/60 bg-red-500/5" : "border-border"}`}
              >
                <p className="text-sm">
                  <strong>{notificationSender(item)}</strong>{" "}
                  {item.kind === "share"
                    ? "shared a session with you"
                    : item.resolved_at
                      ? "requested attention · Resolved"
                      : "needs your attention"}
                </p>
                <Link
                  className="block text-sm font-medium underline"
                  to={`${item.hostId ? "/private" : "/sessions"}/${encodeURIComponent(item.session_id)}`}
                  onClick={() => {
                    if (!item.read_at)
                      action.mutate({
                        id: item.id,
                        hostId: item.hostId,
                        session_id: item.session_id,
                        resolve: false,
                      });
                    setOpen(false);
                  }}
                >
                  {item.session_title || item.session_id}
                </Link>
                {item.message && (
                  <p className="whitespace-pre-wrap break-words text-sm">
                    {item.message}
                  </p>
                )}
                <div className="flex flex-wrap gap-2">
                  {!item.read_at && (
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={action.isPending}
                      onClick={() =>
                        action.mutate({
                          id: item.id,
                          hostId: item.hostId,
                          session_id: item.session_id,
                          resolve: false,
                        })
                      }
                    >
                      Mark read
                    </Button>
                  )}
                  {item.kind === "attention" && !item.resolved_at && (
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
                  )}
                </div>
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
