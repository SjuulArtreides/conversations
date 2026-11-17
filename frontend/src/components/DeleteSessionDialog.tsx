import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { api } from "@/lib/api";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog";
import { Button } from "./ui/button";
import { useToast } from "./ui/toast";

interface DeleteSessionDialogProps {
  id: string;
  title: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function DeleteSessionDialog({
  id,
  title,
  open,
  onOpenChange,
}: DeleteSessionDialogProps) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { toast } = useToast();

  const mutation = useMutation({
    mutationFn: () => api.deleteSession(id),
    onSuccess: () => {
      queryClient.removeQueries({ queryKey: ["session", id] });
      queryClient.invalidateQueries({ queryKey: ["sessions"] });
      toast("Session deleted");
      onOpenChange(false);
      navigate("/", { replace: true });
    },
    onError: (err: Error) => {
      toast(`Failed to delete session: ${err.message}`, "error");
    },
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby="delete-desc">
        <DialogHeader>
          <DialogTitle>Delete session</DialogTitle>
          <DialogDescription id="delete-desc">
            Permanently delete “{title}” and all its messages. This can’t be
            undone.
          </DialogDescription>
        </DialogHeader>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={mutation.isPending}
          >
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={() => mutation.mutate()}
            disabled={mutation.isPending}
          >
            {mutation.isPending ? "Deleting…" : "Delete"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
