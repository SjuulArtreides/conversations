import * as React from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { useEndLiveSession, useRestartSession } from "./HostActions";
import { LiveShareDialog } from "./LiveShareDialog";
import { RenameSessionDialog } from "./RenameSessionDialog";
import { ProjectAssignDialog } from "./ProjectAssignDialog";
import { AttentionDialog } from "./AttentionDialog";
import { Button } from "@/components/ui/button";
import { Chevron } from "@/components/ui/chevron";
import { useAuth } from "@/lib/useAuth";
import type { LiveSession } from "@/lib/types";

interface Props {
  session: Pick<
    LiveSession,
    "id" | "title" | "viewer_is_author" | "viewer_can_write"
  >;
  sessions: LiveSession[];
  currentId: string;
  context?: { x: number; y: number };
  onContextClose?: () => void;
  children?: React.ReactNode;
  onOpenChange?: (open: boolean) => void;
  onCloseAutoFocus?: (event: Event) => void;
}

export const sessionMenuItemClass =
  "cursor-pointer rounded-sm px-3 py-2.5 outline-hidden hover:bg-accent focus:bg-accent data-disabled:cursor-not-allowed data-disabled:opacity-50";

/** The same session actions for the terminal toolbar and a right-clicked tab. */
export function SessionActionsMenu({
  session,
  sessions,
  currentId,
  context,
  onContextClose,
  children,
  onOpenChange,
  onCloseAutoFocus,
}: Props) {
  const [dialog, setDialog] = React.useState<
    "share" | "rename" | "project" | "attention" | null
  >(null);
  const { restart, busy, available } = useRestartSession(session.id);
  const { end, endingId } = useEndLiveSession(sessions, currentId);
  const { user } = useAuth();
  const owner = session.viewer_is_author;
  const title = session.title || session.id;

  if (!owner && !children && !user) return null;

  return (
    <>
      <SessionActionsMenuFrame
        context={context}
        onContextClose={onContextClose}
        onOpenChange={onOpenChange}
        onCloseAutoFocus={onCloseAutoFocus}
        dialogOpen={dialog !== null}
      >
        {owner && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            disabled={!available || busy}
            onSelect={() => {
              void restart();
              if (context) onContextClose?.();
            }}
          >
            {busy ? "Restarting…" : "Restart session"}
          </DropdownMenu.Item>
        )}
        {owner && (
          <DropdownMenu.Item
            className={`${sessionMenuItemClass} text-red-400`}
            disabled={endingId === session.id}
            onSelect={() => {
              void end(session.id);
              if (context) onContextClose?.();
            }}
          >
            {endingId === session.id ? "Ending…" : "End session"}
          </DropdownMenu.Item>
        )}
        {user && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("attention")}
          >
            Ask for attention…
          </DropdownMenu.Item>
        )}
        {owner && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("share")}
          >
            Share session
          </DropdownMenu.Item>
        )}
        {owner && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("rename")}
          >
            Rename session
          </DropdownMenu.Item>
        )}
        {owner && (
          <DropdownMenu.Item
            className={sessionMenuItemClass}
            onSelect={() => setDialog("project")}
          >
            Project
          </DropdownMenu.Item>
        )}
        {children && (
          <>
            <DropdownMenu.Separator className="my-1 h-px bg-border" />
            {children}
          </>
        )}
      </SessionActionsMenuFrame>
      {dialog === "attention" && (
        <AttentionDialog
          sessionId={session.id}
          title={title}
          onOpenChange={(next) => {
            if (!next) {
              setDialog(null);
              onContextClose?.();
            }
          }}
        />
      )}
      {dialog === "project" && (
        <ProjectAssignDialog
          sessionId={session.id}
          onOpenChange={(next) => {
            if (!next) {
              setDialog(null);
              onContextClose?.();
            }
          }}
        />
      )}
      {dialog === "share" && (
        <LiveShareDialog
          liveId={session.id}
          title={title}
          open
          onOpenChange={(next) => {
            if (!next) {
              setDialog(null);
              onContextClose?.();
            }
          }}
        />
      )}
      {dialog === "rename" && (
        <RenameSessionDialog
          id={session.id}
          title={title}
          open
          onOpenChange={(next) => {
            if (!next) {
              setDialog(null);
              onContextClose?.();
            }
          }}
        />
      )}
    </>
  );
}

export function SessionActionsMenuFrame({
  children,
  context,
  onContextClose,
  onOpenChange,
  onCloseAutoFocus,
  dialogOpen = false,
}: Pick<
  Props,
  | "children"
  | "context"
  | "onContextClose"
  | "onOpenChange"
  | "onCloseAutoFocus"
> & { dialogOpen?: boolean }) {
  const [open, setOpen] = React.useState(Boolean(context));
  return (
    <DropdownMenu.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        onOpenChange?.(next);
      }}
    >
      <DropdownMenu.Trigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          aria-label="Actions"
          title="Actions"
          style={
            context
              ? {
                  position: "fixed",
                  left: context.x,
                  top: context.y,
                  width: 0,
                  height: 0,
                  padding: 0,
                  border: 0,
                }
              : undefined
          }
          className="gap-1"
        >
          {context ? null : (
            <>
              Actions
              <Chevron />
            </>
          )}
        </Button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align={context ? "start" : "end"}
          sideOffset={6}
          onCloseAutoFocus={(event) => {
            if (dialogOpen) event.preventDefault();
            else onCloseAutoFocus?.(event);
          }}
          onEscapeKeyDown={() => {
            if (context && !dialogOpen) onContextClose?.();
          }}
          onPointerDownOutside={() => {
            if (context && !dialogOpen) onContextClose?.();
          }}
          className="z-70 flex max-h-[calc(100vh-2rem)] min-w-56 flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 text-sm text-popover-foreground shadow-md"
        >
          {children}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
