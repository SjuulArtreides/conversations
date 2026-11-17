import * as React from "react";
import { hostControl, HostControlError } from "@/lib/hostControl";
import { useToast } from "@/components/ui/toast";

/** The host's file-drop limits: 16 files, 9 MiB combined raw bytes. */
export const TERMINAL_DROP_MAX_FILES = 16;
export const TERMINAL_DROP_MAX_BYTES = 9 * 1024 * 1024;

export type TerminalDropStatus = "reading" | "sending" | null;

const FOLDER_MESSAGE =
  "Folders can't be attached. Drop individual files instead.";
const NETWORK_MESSAGE =
  "Could not reach your host privately, so the files may not have been delivered. Check the prompt before dropping them again.";
const BUSY_MESSAGE =
  "Files are still being sent. Wait for them to finish, then drop again.";

/** Whether a drag carries files (as opposed to text or a link). */
export function isFileDrag(dt: DataTransfer | null | undefined): boolean {
  return !!dt && Array.from(dt.types ?? []).includes("Files");
}

type DropPlan = { files: File[] } | { error: string } | null;

/**
 * Validate a drop synchronously, before any file bytes are read. Returns null
 * when there is nothing to send. DataTransfer is only readable during the
 * event, so everything needed is captured here.
 */
export function planFileDrop(dt: DataTransfer): DropPlan {
  const items = Array.from(dt.items ?? []);
  const hasDirectory = items.some((item) => {
    if (item.kind !== "file") return false;
    const entry = item.webkitGetAsEntry?.();
    return !!entry?.isDirectory;
  });
  if (hasDirectory) return { error: FOLDER_MESSAGE };

  const files = Array.from(dt.files ?? []);
  if (files.length === 0) return null;
  if (files.length > TERMINAL_DROP_MAX_FILES) {
    return {
      error: `You can attach at most ${TERMINAL_DROP_MAX_FILES} files at once. Drop fewer files.`,
    };
  }
  const total = files.reduce((sum, f) => sum + f.size, 0);
  if (total > TERMINAL_DROP_MAX_BYTES) {
    return {
      error: `These files total more than ${TERMINAL_DROP_MAX_BYTES / 1024 / 1024} MiB. Drop fewer or smaller files.`,
    };
  }
  return { files };
}

function sendFailureMessage(err: unknown): string {
  if (err instanceof HostControlError && err.code === "transport")
    return NETWORK_MESSAGE;
  if (err instanceof HostControlError && err.message.trim()) return err.message;
  return NETWORK_MESSAGE;
}

interface Options {
  sessionId: string;
  /** The paired host that holds the session; the active host when omitted. */
  hostId?: string;
  /** Why a drop can't be accepted right now, or null when it can. */
  blockedReason: string | null;
  /** Called once the host has acknowledged delivery. */
  onSent?: () => void;
}

/**
 * File drops for the live terminal. File drags are always claimed so the
 * browser never navigates, but readiness is re-checked after the async read
 * and nothing is retried (a failed send may follow a delivery).
 */
export function useTerminalFileDrop({
  sessionId,
  hostId,
  blockedReason,
  onSent,
}: Options) {
  const { toast } = useToast();
  const [dragging, setDragging] = React.useState(false);
  const [status, setStatus] = React.useState<TerminalDropStatus>(null);

  const idRef = React.useRef(sessionId);
  idRef.current = sessionId;
  const hostRef = React.useRef(hostId);
  hostRef.current = hostId;
  const blockedRef = React.useRef(blockedReason);
  blockedRef.current = blockedReason;
  const onSentRef = React.useRef(onSent);
  onSentRef.current = onSent;
  const toastRef = React.useRef(toast);
  toastRef.current = toast;

  const depthRef = React.useRef(0);
  const inFlightRef = React.useRef(false);
  const generationRef = React.useRef(0);
  const abortRef = React.useRef<AbortController | null>(null);

  React.useEffect(() => {
    return () => {
      generationRef.current += 1;
      abortRef.current?.abort();
      abortRef.current = null;
      inFlightRef.current = false;
      depthRef.current = 0;
      setDragging(false);
      setStatus(null);
    };
  }, [sessionId]);

  const upload = React.useCallback(async (files: File[]) => {
    const generation = generationRef.current;
    const id = idRef.current;
    const stale = () =>
      generation !== generationRef.current || id !== idRef.current;
    const controller = new AbortController();
    abortRef.current = controller;
    inFlightRef.current = true;
    setStatus("reading");
    try {
      let buffers: ArrayBuffer[];
      try {
        buffers = await Promise.all(files.map((f) => f.arrayBuffer()));
      } catch {
        if (!stale())
          toastRef.current(
            "Could not read the dropped files. They may have been moved or deleted; drop them again.",
            "error",
          );
        return;
      }
      if (stale()) return;
      const reason = blockedRef.current;
      if (reason) {
        toastRef.current(reason, "error");
        return;
      }
      setStatus("sending");
      try {
        // Only over the private channel; the host checks every signed hash.
        await hostControl.sendFiles(
          id,
          files.map((f, i) => ({
            name: f.name || "file",
            bytes: new Uint8Array(buffers[i]),
          })),
          { signal: controller.signal },
          hostRef.current,
        );
      } catch (err) {
        if (!stale() && !controller.signal.aborted)
          toastRef.current(sendFailureMessage(err), "error");
        return;
      }
      if (!stale()) onSentRef.current?.();
    } finally {
      if (!stale()) {
        inFlightRef.current = false;
        abortRef.current = null;
        setStatus(null);
      }
    }
  }, []);

  const onDragEnter = React.useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e.dataTransfer)) return;
    e.preventDefault();
    e.stopPropagation();
    depthRef.current += 1;
    setDragging(true);
  }, []);

  const onDragOver = React.useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e.dataTransfer)) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = blockedRef.current ? "none" : "copy";
  }, []);

  const onDragLeave = React.useCallback((e: React.DragEvent) => {
    if (!isFileDrag(e.dataTransfer)) return;
    e.stopPropagation();
    depthRef.current = Math.max(0, depthRef.current - 1);
    if (depthRef.current === 0) setDragging(false);
  }, []);

  const onDrop = React.useCallback(
    (e: React.DragEvent) => {
      if (!isFileDrag(e.dataTransfer)) return;
      e.preventDefault();
      e.stopPropagation();
      depthRef.current = 0;
      setDragging(false);

      const blocked = blockedRef.current;
      if (blocked) {
        toast(blocked, "error");
        return;
      }
      if (inFlightRef.current) {
        toast(BUSY_MESSAGE, "error");
        return;
      }
      const plan = planFileDrop(e.dataTransfer);
      if (!plan) return;
      if ("error" in plan) {
        toast(plan.error, "error");
        return;
      }
      void upload(plan.files);
    },
    [toast, upload],
  );

  return {
    dragging,
    status,
    handlers: { onDragEnter, onDragOver, onDragLeave, onDrop },
  };
}
