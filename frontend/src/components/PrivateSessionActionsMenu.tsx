import { MigrateSessionDialog } from "./MigrateSessionDialog";
import { AttentionDialog } from "./AttentionDialog";
import { ProjectAssignDialog } from "./ProjectAssignDialog";
import {
  restartPrivateSession,
  privateTerminalGenerationKey,
} from "@/lib/privateSessionLifecycle";
import * as React from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { useQueryClient } from "@tanstack/react-query";
import {
  SessionActionsMenuFrame,
  sessionMenuItemClass,
} from "./SessionActionsMenu";
import { RenameSessionDialog } from "./RenameSessionDialog";
import { SessionShareDialog } from "./SessionShareDialog";
import { useToast } from "./ui/toast";
import { hostControl } from "@/lib/hostControl";
import type { SessionEntry } from "@/lib/privateChannel";

/** Host authority stays in the transport; no server LiveSession is fabricated. */
export function PrivateSessionActionsMenu({
  session,
  hostId,
  children,
  context,
  onContextClose,
  onOpenChange,
  onCloseAutoFocus,
}: {
  session: SessionEntry;
  hostId: string;
  children?: React.ReactNode;
  context?: { x: number; y: number };
  onContextClose?: () => void;
  onOpenChange?: (open: boolean) => void;
  onCloseAutoFocus?: (event: Event) => void;
}) {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const [dialog, setDialog] = React.useState<
    "rename" | "share" | "project" | "attention" | "migrate" | null
  >(null);
  const [busy, setBusy] = React.useState(false);
  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: ["private", hostId] });
  const close = (open: boolean) => {
    if (!open) {
      setDialog(null);
      onContextClose?.();
    }
  };
  const title = session.title || session.id;
  async function lifecycle(restart = false) {
    if (busy) return;
    setBusy(true);
    try {
      if (restart) {
        await restartPrivateSession(hostId, session.id);
        queryClient.setQueryData<number>(
          privateTerminalGenerationKey(hostId, session.id),
          (previous = 0) => previous + 1,
        );
      } else if (session.running)
        await hostControl.endSession(session.id, hostId);
      else await hostControl.reopenSession(session.id, hostId);
      await refresh();
      onContextClose?.();
    } catch (error) {
      toast((error as Error).message, "error");
    } finally {
      setBusy(false);
    }
  }
  return (
    <>
      <SessionActionsMenuFrame
        context={context}
        onContextClose={onContextClose}
        onOpenChange={onOpenChange}
        onCloseAutoFocus={onCloseAutoFocus}
        dialogOpen={dialog !== null}
      >
        {session.running && session.reopenable !== false && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            disabled={busy}
            onSelect={() => void lifecycle(true)}
          >
            {busy ? "Restarting…" : "Restart session"}
          </DropdownMenu.Item>
        )}
        {(session.running || session.reopenable !== false) && (
          <DropdownMenu.Item
            className={`${sessionMenuItemClass}${session.running ? " text-red-400" : ""}`}
            disabled={busy}
            onSelect={() => void lifecycle()}
          >
            {busy
              ? session.running
                ? "Ending…"
                : "Reopening…"
              : session.running
                ? "End session"
                : "Re-open as live"}
          </DropdownMenu.Item>
        )}
        {session.agent !== "zsh" && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("migrate")}
          >
            Migrate…
          </DropdownMenu.Item>
        )}
        <DropdownMenu.Item
          className={sessionMenuItemClass}
          onSelect={() => setDialog("share")}
        >
          Share…
        </DropdownMenu.Item>
        <DropdownMenu.Item
          className={sessionMenuItemClass}
          onSelect={() => setDialog("rename")}
        >
          Rename session
        </DropdownMenu.Item>
        {
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("project")}
          >
            Project
          </DropdownMenu.Item>
        }
        <DropdownMenu.Item
          className={sessionMenuItemClass}
          onSelect={() => setDialog("attention")}
        >
          Ask for attention
        </DropdownMenu.Item>
        {children && (
          <>
            <DropdownMenu.Separator className="my-1 h-px bg-border" />
            {children}
          </>
        )}
      </SessionActionsMenuFrame>
      {dialog === "migrate" && (
        <MigrateSessionDialog
          session={session}
          hostId={hostId}
          onOpenChange={close}
        />
      )}
      {dialog === "attention" && (
        <AttentionDialog
          sessionId={session.id}
          hostId={hostId}
          title={title}
          onOpenChange={close}
        />
      )}
      {dialog === "project" && (
        <ProjectAssignDialog
          sessionId={session.id}
          hostId={hostId}
          onOpenChange={close}
        />
      )}
      {dialog === "rename" && (
        <RenameSessionDialog
          id={session.id}
          title={title}
          open
          onOpenChange={close}
          onRename={async (next) => {
            const effective = await hostControl.renameSession(
              session.id,
              next,
              hostId,
            );
            await refresh();
            return effective;
          }}
        />
      )}
      {dialog === "share" && (
        <SessionShareDialog
          hostId={hostId}
          sessionId={session.id}
          title={title}
          open
          onOpenChange={close}
        />
      )}
    </>
  );
}
