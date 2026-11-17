import { hostControl } from "@/lib/hostControl";
import { readHistory } from "@/lib/privateChannel";
import type { Message } from "@/lib/types";
import type { MermaidSource } from "@/lib/mermaidSources";
import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { useLiveStream } from "@/lib/useLiveStream";
import { mermaidSources } from "@/lib/mermaidSources";
import { renderMermaidSvg } from "@/lib/mermaid";
import { MermaidViewport } from "./MermaidPopup";
import { Button } from "./ui/button";
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
  DialogHeader,
} from "./ui/dialog";

/**
 * Controlled diagram viewer. The trigger lives in the Display dropdown menu
 * (see LiveTerminal's DisplayMenuItems); this component owns only the lazily
 * mounted dialog content. Source bodies are requested only while open — keep
 * the open guard around the content children or we'll hit the host every
 * render.
 */
export function TerminalDiagrams({
  sessionId,
  hostId,
  open,
  onOpenChange,
}: {
  sessionId: string;
  hostId?: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open &&
        (hostId ? (
          <PrivateDiagramContent
            key={`${hostId}:${sessionId}`}
            sessionId={sessionId}
            hostId={hostId}
          />
        ) : (
          <DiagramContent sessionId={sessionId} />
        ))}
    </Dialog>
  );
}

function DiagramContent({ sessionId }: { sessionId: string }) {
  const { messages, status } = useLiveStream(sessionId);
  const sources = React.useMemo(() => mermaidSources(messages), [messages]);
  return (
    <DiagramView sessionId={sessionId} sources={sources} status={status} />
  );
}
function PrivateDiagramContent({
  sessionId,
  hostId,
}: {
  sessionId: string;
  hostId: string;
}) {
  const [sources, setSources] = React.useState<MermaidSource[]>([]);
  const [status, setStatus] = React.useState<{
    kind: string;
    message?: string;
  }>({ kind: "loading" });
  React.useEffect(() => {
    const controller = new AbortController();
    void hostControl
      .privateRead(hostId, (channel) =>
        readHistory(channel, sessionId, {
          signal: controller.signal,
          onPage: (page) => {
            const found = mermaidSources(
              page.map((entry) => entry.message as unknown as Message),
            );
            if (found.length && !controller.signal.aborted)
              setSources((previous) => [...previous, ...found]);
          },
        }),
      )
      .then(() => {
        if (!controller.signal.aborted) setStatus({ kind: "complete" });
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted)
          setStatus({ kind: "error", message: (error as Error).message });
      });
    return () => controller.abort();
  }, [hostId, sessionId]);
  return (
    <DiagramView
      sessionId={`${hostId}:${sessionId}`}
      sources={sources}
      status={status}
    />
  );
}
function DiagramView({
  sessionId,
  sources,
  status,
}: {
  sessionId: string;
  sources: MermaidSource[];
  status: { kind: string; message?: string };
}) {
  const [chosen, setChosen] = React.useState("");
  const selected =
    sources.find((entry) => entry.id === chosen) ?? sources.at(-1);
  const reactId = React.useId();
  const renderId = `terminal-mermaid-${reactId.replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const diagram = useQuery({
    queryKey: ["terminal-mermaid", sessionId, selected?.source],
    queryFn: () => renderMermaidSvg(renderId, selected!.source),
    enabled: Boolean(selected),
    staleTime: Infinity,
    retry: false,
  });
  return (
    <DialogContent className="flex h-[90dvh] w-[calc(100vw-2rem)] max-w-none flex-col p-4">
      <DialogHeader className="mb-0 pr-8">
        <DialogTitle>Terminal diagrams</DialogTitle>
        <DialogDescription>
          Agent diagrams from this live session. Zoom in or use 1:1, then scroll
          to read wide diagrams.
        </DialogDescription>
      </DialogHeader>
      {status.kind === "error" && (
        <p role="alert" className="text-sm text-red-500">
          {status.message}
        </p>
      )}
      {sources.length > 1 && (
        <label className="flex items-center gap-2 text-sm">
          Diagram
          <select
            aria-label="Choose diagram"
            value={selected?.id}
            onChange={(event) => setChosen(event.target.value)}
            className="min-w-0 rounded border border-border bg-card px-2 py-1"
          >
            {sources
              .map((entry, index) => (
                <option key={entry.id} value={entry.id}>
                  Diagram {index + 1}
                  {index === sources.length - 1 ? " (latest)" : ""}
                </option>
              ))
              .reverse()}
          </select>
        </label>
      )}
      {!selected && status.kind !== "error" && (
        <p role="status" className="p-4 text-sm text-muted-foreground">
          No complete Mermaid diagrams yet. Waiting for Mermaid source from the
          agent.
        </p>
      )}
      {selected && diagram.isPending && (
        <p role="status" className="p-4 text-sm text-muted-foreground">
          Rendering diagram…
        </p>
      )}
      {diagram.isError && (
        <div role="alert" className="p-4 text-sm">
          <p>{diagram.error.message}</p>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => void diagram.refetch()}
          >
            Retry
          </Button>
        </div>
      )}
      {selected && diagram.data && (
        <MermaidViewport
          key={selected.id}
          svg={diagram.data}
          source={selected.source}
        />
      )}
    </DialogContent>
  );
}
