import { useLocalHost } from "@/lib/localHost";
import { useRef } from "react";
import {
  privateAttentionRecipients,
  requestPrivateAttention,
} from "@/lib/privateAttention";
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { useToast } from "./ui/toast";

export function AttentionDialog({
  sessionId,
  hostId,
  title,
  onOpenChange,
}: {
  sessionId: string;
  hostId?: string;
  title: string;
  onOpenChange: (open: boolean) => void;
}) {
  const local = useLocalHost();
  const allowed = !local.local || local.authenticated;
  const attempt = useRef<{ payload: string; id: string } | null>(null);
  const [recipient, setRecipient] = useState("");
  const [message, setMessage] = useState("");
  const client = useQueryClient();
  const { toast } = useToast();
  const people = useQuery({
    queryKey: ["attention-recipients", hostId ?? "central", sessionId],
    enabled: allowed,
    queryFn: async () =>
      hostId
        ? (await privateAttentionRecipients(hostId, sessionId)).map(
            (person) => ({
              id: JSON.stringify(person.subject),
              label: person.label,
              subject: person.subject,
            }),
          )
        : (await api.attentionRecipients(sessionId)).map((person) => ({
            id: person.email,
            label: person.display_name
              ? `${person.display_name} (${person.email})`
              : person.email,
            subject: undefined,
          })),
  });
  const selected =
    recipient || (people.data?.length === 1 ? people.data[0].id : "");
  const mutation = useMutation({
    mutationFn: async () => {
      if (!allowed) throw new Error("Unlock this host first.");
      if (!hostId)
        return api.requestAttention(sessionId, selected, message.trim());
      const person = people.data?.find((person) => person.id === selected);
      if (!person?.subject) throw new Error("Choose an approved recipient.");
      const payload = JSON.stringify({
        hostId,
        sessionId,
        recipient: person.subject,
        message: message.trim(),
      });
      if (attempt.current?.payload !== payload)
        attempt.current = { payload, id: crypto.randomUUID() };
      await requestPrivateAttention(
        hostId,
        sessionId,
        person.subject,
        message.trim(),
        attempt.current.id,
      );
    },
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ["notifications"] });
      toast("Attention requested");
      onOpenChange(false);
    },
  });
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Ask for attention</DialogTitle>
          <DialogDescription>
            Ask a colleague to look at “{title}”.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (selected && message.trim()) mutation.mutate();
          }}
        >
          <label className="block space-y-1 text-sm font-medium">
            <span>Who needs to see this?</span>
            <select
              required
              value={selected}
              onChange={(event) => setRecipient(event.target.value)}
              disabled={people.isPending || people.isError}
              className="w-full rounded-md border border-input bg-background p-2"
            >
              <option value="">Choose a colleague</option>
              {people.data?.map((person) => (
                <option key={person.id} value={person.id}>
                  {person.label}
                </option>
              ))}
            </select>
          </label>
          {people.isPending && (
            <p className="mt-2 text-sm text-muted-foreground">
              Loading colleagues…
            </p>
          )}
          {people.isError && (
            <p role="alert" className="mt-2 text-sm text-destructive">
              Could not load colleagues.{" "}
              <button
                type="button"
                onClick={() => void people.refetch()}
                className="underline"
              >
                Retry
              </button>
            </p>
          )}
          {people.data?.length === 0 && (
            <p className="mt-2 text-sm text-muted-foreground">
              Share this session with a colleague first.
            </p>
          )}
          <label className="mt-4 block space-y-1 text-sm font-medium">
            <span>What do you need help with?</span>
            <textarea
              required
              maxLength={2000}
              value={message}
              onChange={(event) => setMessage(event.target.value)}
              className="min-h-24 w-full rounded-md border border-input bg-background p-2 font-normal"
            />
          </label>
          {mutation.error && (
            <p role="alert" className="text-sm text-destructive">
              {mutation.error.message}
            </p>
          )}
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={
                !selected ||
                !message.trim() ||
                mutation.isPending ||
                people.isError
              }
            >
              {mutation.isPending ? "Sending…" : "Send request"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
