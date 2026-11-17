import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import type { Session, SessionPublication, Visibility } from "@/lib/types";
import { api } from "@/lib/api";
import { usePairedHosts } from "@/lib/hostPairing";
import { SessionShareDialog } from "./SessionShareDialog";
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
import { LiveShareDialog } from "./LiveShareDialog";
import { EmailAutocomplete } from "./EmailAutocomplete";
import { useToast } from "./ui/toast";

interface ShareDialogProps {
  session: Pick<
    Session,
    "id" | "title" | "visibility" | "shared_emails" | "publication"
  >;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function ShareDialog(props: ShareDialogProps) {
  // A session a host publishes is shared only by that host's signed operation;
  // the server's legacy sharing writer refuses it.
  if (props.session.publication) {
    return (
      <PublishedShareDialog
        {...props}
        publication={props.session.publication}
      />
    );
  }
  return <LegacyShareDialog {...props} />;
}

function PublishedShareDialog({
  session,
  publication,
  open,
  onOpenChange,
}: ShareDialogProps & { publication: SessionPublication }) {
  // The server names the publishing host (metadata, not authority). Controls
  // are offered only for a host this browser paired; the host still verifies
  // the passkey delegation and its own grant for every change.
  const publisher = publication.host_id;
  const paired = usePairedHosts().hosts.some((h) => h.id === publisher);
  if (paired && publisher) {
    // One Share dialog everywhere: a published
    // session gets the same read levels, key offers and trust ceremony as
    // the private session page — driven by its publishing host.
    return (
      <SessionShareDialog
        hostId={publisher}
        sessionId={session.id}
        title={session.title}
        open={open}
        onOpenChange={onOpenChange}
      />
    );
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby="share-desc">
        <DialogHeader>
          <DialogTitle>Share session</DialogTitle>
          <DialogDescription id="share-desc">
            {publisher
              ? `“${session.title}” is published by a host this browser has not paired. To change who reads it, pair that host in Settings; the host still checks its own grants.`
              : `“${session.title}” is published by its host. Only its owner can change who reads it, from that host.`}
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function LegacyShareDialog({ session, open, onOpenChange }: ShareDialogProps) {
  const [visibility, setVisibility] = React.useState<Visibility>(
    session.visibility,
  );
  const [emails, setEmails] = React.useState<string[]>(
    session.shared_emails ?? [],
  );
  const [personAccessOpen, setPersonAccessOpen] = React.useState(false);
  const [reason, setReason] = React.useState("");
  const queryClient = useQueryClient();
  const { toast } = useToast();

  // Reset local state to the session's current sharing whenever the dialog opens.
  React.useEffect(() => {
    if (!open) setPersonAccessOpen(false);
    if (open) {
      setVisibility(session.visibility);
      setEmails(session.shared_emails ?? []);
    }
  }, [open, session.visibility, session.shared_emails]);

  const mutation = useMutation({
    mutationFn: () =>
      api.setSharing(session.id, {
        visibility,
        ...(reason.trim() ? { reason: reason.trim() } : {}),
        emails: visibility === "restricted" ? emails : [],
      }),
    onSuccess: (data) => {
      queryClient.setQueryData<Session>(["session", session.id], (prev) =>
        prev
          ? {
              ...prev,
              visibility: data.visibility,
              shared_emails: data.emails,
            }
          : prev,
      );
      queryClient.invalidateQueries({ queryKey: ["session", session.id] });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      toast("Sharing updated");
      onOpenChange(false);
    },
    onError: (err: Error) => {
      toast(`Failed to update sharing: ${err.message}`, "error");
    },
  });

  React.useEffect(() => {
    if (open) setReason("");
  }, [open]);

  return (
    <>
      <LiveShareDialog
        liveId={session.id}
        title={session.title}
        open={open && personAccessOpen}
        onOpenChange={setPersonAccessOpen}
      />
      <Dialog open={open && !personAccessOpen} onOpenChange={onOpenChange}>
        <DialogContent aria-describedby="share-desc">
          <DialogHeader>
            <DialogTitle>Share session</DialogTitle>
            <DialogDescription id="share-desc">
              Choose who can read “{session.title}”.
            </DialogDescription>
          </DialogHeader>

          <RadioGroup
            value={visibility}
            onValueChange={(v) => setVisibility(v as Visibility)}
            aria-label="Visibility"
          >
            <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-border p-3 transition-colors hover:bg-accent/50 has-checked:border-primary/50 has-checked:bg-primary/5">
              <RadioGroupItem
                value="organization"
                id="vis-org"
                className="mt-0.5"
              />
              <span>
                <span className="block text-sm font-medium">
                  🌐 Entire organization
                </span>
                <span className="block text-xs text-muted-foreground">
                  Anyone in your org can read this session.
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-border p-3 transition-colors hover:bg-accent/50 has-checked:border-primary/50 has-checked:bg-primary/5">
              <RadioGroupItem
                value="restricted"
                id="vis-restricted"
                className="mt-0.5"
              />
              <span>
                <span className="block text-sm font-medium">
                  🔒 Specific people
                </span>
                <span className="block text-xs text-muted-foreground">
                  The owner, people added below, and people granted read access
                  in person sharing.
                </span>
              </span>
            </label>
          </RadioGroup>

          {visibility === "restricted" && (
            <div className="mt-4">
              <EmailAutocomplete
                selected={emails}
                onAdd={(e) => setEmails((prev) => [...prev, e])}
                onRemove={(e) =>
                  setEmails((prev) => prev.filter((x) => x !== e))
                }
              />
            </div>
          )}

          <Button variant="outline" onClick={() => setPersonAccessOpen(true)}>
            Manage person access and communication
          </Button>

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
              disabled={mutation.isPending}
            >
              {mutation.isPending ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
