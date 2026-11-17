import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { LIVE_SESSIONS_QUERY_KEY } from "@/lib/useLiveSessions";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { useToast } from "./ui/toast";

interface RenameSessionDialogProps {
  id: string;
  /** Transport adapter for host-owned sessions. Returns the effective title. */
  onRename?: (title: string) => Promise<string>;
  /** The session's current (effective) title, used to seed the field. */
  title: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Rename a session via a DB-only override (`PUT /api/sessions/{id}/title`).
 * The on-disk transcript is never modified — submitting a blank title clears
 * the override and reverts to the parsed transcript title. On success the
 * session + list queries are invalidated so the new title shows everywhere.
 */
export function RenameSessionDialog({
  id,
  title,
  open,
  onOpenChange,
  onRename,
}: RenameSessionDialogProps) {
  const queryClient = useQueryClient();
  const { toast } = useToast();
  const [value, setValue] = React.useState(title);

  // Reseed the field whenever the dialog (re)opens or the title changes.
  React.useEffect(() => {
    if (open) setValue(title);
  }, [open, title]);

  const mutation = useMutation({
    mutationFn: (next: string) =>
      onRename ? onRename(next) : api.setTitle(id, next),
    onSuccess: (effective) => {
      queryClient.invalidateQueries({ queryKey: ["session", id] });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      // A live session renamed from its live view: refresh the live poll so the
      // overlaid title (server-side `title_override`) shows immediately rather
      // than after the next 5s tick. A no-op for stored sessions.
      queryClient.invalidateQueries({ queryKey: LIVE_SESSIONS_QUERY_KEY });
      toast(`Renamed to “${effective}”`);
      onOpenChange(false);
    },
    onError: (err: Error) => {
      toast(`Failed to rename session: ${err.message}`, "error");
    },
  });

  const trimmed = value.trim();

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Rename session</DialogTitle>
          <DialogDescription>
            Set a custom title for this session. This is stored only in the
            database — the original transcript file is never modified. Clear the
            field to revert to the parsed title.
          </DialogDescription>
        </DialogHeader>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (!mutation.isPending) mutation.mutate(trimmed);
          }}
        >
          <Input
            autoFocus
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder="Session title"
            disabled={mutation.isPending}
            aria-label="Session title"
          />

          <DialogFooter className="mt-4">
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={mutation.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
