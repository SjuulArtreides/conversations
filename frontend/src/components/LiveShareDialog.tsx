import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { LiveShare } from "@/lib/types";
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
import { RadioGroup, RadioGroupItem } from "./ui/radio-group";
import { Button } from "./ui/button";
import { EmailAutocomplete } from "./EmailAutocomplete";
import { useToast } from "./ui/toast";

/** A share with every capability explicit, as the dialog edits and saves it. */
type PersonShare = Required<Omit<LiveShare, "communication_only">>;

type TranscriptAccess = "none" | "read" | "write";

/** Fill fields an older server omits with the capabilities they implied. */
function normalize(share: LiveShare): PersonShare {
  const legacyNoRead = share.communication_only === true;
  return {
    email: share.email,
    can_read: share.can_read ?? !legacyNoRead,
    can_write: share.can_write,
    can_communicate: share.can_communicate ?? (share.can_write || legacyNoRead),
  };
}

function transcriptAccess(share: PersonShare): TranscriptAccess {
  if (share.can_write) return "write";
  return share.can_read ? "read" : "none";
}

interface LiveShareDialogProps {
  /** The session being shared (live or stored). */
  liveId: string;
  title: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Owner-only person sharing. Transcript access (none / read / read + write) and
 * communication are independent choices per person.
 */
export function LiveShareDialog({
  liveId,
  title,
  open,
  onOpenChange,
}: LiveShareDialogProps) {
  const [shares, setShares] = React.useState<PersonShare[]>([]);
  const [reason, setReason] = React.useState("");
  const queryClient = useQueryClient();
  const { toast } = useToast();

  const sharingQuery = useQuery({
    queryKey: ["live-sharing", liveId],
    queryFn: () => api.getLiveSharing(liveId),
    enabled: open,
  });

  // Seed local state from the server whenever the dialog opens (or the loaded
  // shares change), so edits start from the persisted list.
  React.useEffect(() => {
    if (open && sharingQuery.data) {
      setShares(sharingQuery.data.map(normalize));
    }
  }, [open, sharingQuery.data]);

  const mutation = useMutation({
    mutationFn: () => {
      // An older server ignores can_read; communication_only keeps it from
      // granting transcript access to a person who should have none.
      const payload = shares.map((s) => ({
        ...s,
        communication_only: !s.can_read,
      }));
      return reason.trim()
        ? api.setLiveSharing(liveId, payload, reason.trim())
        : api.setLiveSharing(liveId, payload);
    },
    onSuccess: (data) => {
      setShares(data.map(normalize));
      queryClient.invalidateQueries({ queryKey: ["live-sharing", liveId] });
      queryClient.invalidateQueries({ queryKey: LIVE_SESSIONS_QUERY_KEY });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      queryClient.invalidateQueries({ queryKey: ["session", liveId] });
      toast("Sharing updated");
      onOpenChange(false);
    },
    onError: (err: Error) => {
      toast(`Failed to update sharing: ${err.message}`, "error");
    },
  });

  const selected = shares.map((s) => s.email);

  const addEmail = (email: string) => {
    setShares((prev) =>
      prev.some((s) => s.email === email)
        ? prev
        : [
            ...prev,
            { email, can_read: true, can_write: false, can_communicate: false },
          ],
    );
  };

  const removeEmail = (email: string) => {
    setShares((prev) => prev.filter((s) => s.email !== email));
  };

  const update = (email: string, change: Partial<PersonShare>) => {
    setShares((prev) =>
      prev.map((s) => (s.email === email ? { ...s, ...change } : s)),
    );
  };

  const setTranscriptAccess = (email: string, access: TranscriptAccess) =>
    update(email, {
      can_read: access !== "none",
      can_write: access === "write",
    });

  const withoutAccess = shares.filter((s) => !s.can_read && !s.can_communicate);

  React.useEffect(() => {
    if (open) setReason("");
  }, [open]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby="live-share-desc">
        <DialogHeader>
          <DialogTitle>Share session</DialogTitle>
          <DialogDescription id="live-share-desc">
            Choose transcript access to “{title}” and, separately, whether each
            person can communicate with it. Communicate lets their sessions
            exchange agent messages with this session; it grants no transcript
            or terminal access.
          </DialogDescription>
        </DialogHeader>

        {sharingQuery.isError && (
          <p role="alert" className="text-sm text-destructive">
            Could not load sharing. Close and reopen to retry.
          </p>
        )}

        <EmailAutocomplete
          selected={selected}
          onAdd={addEmail}
          onRemove={removeEmail}
        />

        {shares.length > 0 && (
          <ul className="mt-4 space-y-2" aria-label="Share levels">
            {shares.map((share) => (
              <li
                key={share.email}
                className="flex flex-col gap-3 rounded-lg border border-border p-3"
              >
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <span className="text-sm font-medium break-all">
                    {share.email}
                  </span>
                  <RadioGroup
                    value={transcriptAccess(share)}
                    onValueChange={(v) =>
                      setTranscriptAccess(share.email, v as TranscriptAccess)
                    }
                    aria-label={`Transcript access for ${share.email}`}
                    className="flex flex-wrap gap-x-3 gap-y-2"
                  >
                    <label className="flex cursor-pointer items-center gap-1.5 py-1 text-xs">
                      <RadioGroupItem value="none" id={`none-${share.email}`} />
                      No access
                    </label>
                    <label className="flex cursor-pointer items-center gap-1.5 py-1 text-xs">
                      <RadioGroupItem value="read" id={`read-${share.email}`} />
                      Read
                    </label>
                    <label className="flex cursor-pointer items-center gap-1.5 py-1 text-xs">
                      <RadioGroupItem
                        value="write"
                        id={`write-${share.email}`}
                      />
                      Read + write
                    </label>
                  </RadioGroup>
                </div>

                <label className="flex cursor-pointer items-start gap-2 text-xs">
                  <input
                    type="checkbox"
                    checked={share.can_communicate}
                    onChange={(event) =>
                      update(share.email, {
                        can_communicate: event.target.checked,
                      })
                    }
                    aria-describedby={`communicate-desc-${share.email}`}
                    className="mt-0.5 h-4 w-4 accent-primary"
                  />
                  <span>
                    <span className="block font-medium">Communicate</span>
                    <span
                      id={`communicate-desc-${share.email}`}
                      className="block text-muted-foreground"
                    >
                      Their sessions can discover and message this session.
                    </span>
                  </span>
                </label>

                {/* Write shares no longer type into anything by themselves:
                    terminal control requires the owner's locally approved key
                    access on the host (signed operations), which this dialog
                    does not grant. */}
                {share.can_write && (
                  <p className="text-[11px] leading-snug text-amber-600 dark:text-amber-400">
                    Can request replies and terminal input, which the session
                    owner&apos;s host accepts only when it also holds locally
                    approved key access for you.
                  </p>
                )}
              </li>
            ))}
          </ul>
        )}

        {withoutAccess.length > 0 && (
          <p role="alert" className="mt-2 text-sm text-destructive">
            {withoutAccess.map((s) => s.email).join(", ")}{" "}
            {withoutAccess.length === 1 ? "has" : "have"} no access. Choose
            Read, Read + write, or Communicate, or remove them.
          </p>
        )}

        <label className="mt-4 block space-y-1 text-sm font-medium">
          <span>Why are you sharing this?</span>
          <textarea
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            maxLength={2000}
            placeholder="Optional context for the people you add"
            className="min-h-20 w-full rounded-md border border-input bg-background p-2 text-sm font-normal"
          />
        </label>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={mutation.isPending}
          >
            Cancel
          </Button>
          <Button
            onClick={() => mutation.mutate()}
            disabled={
              mutation.isPending ||
              withoutAccess.length > 0 ||
              sharingQuery.isLoading ||
              sharingQuery.isError
            }
          >
            {mutation.isPending ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
