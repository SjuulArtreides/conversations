import * as React from "react";
import { cn } from "@/lib/utils";
import { CodeBlock, CodeCopyButton } from "./CodeBlock";
import { Dialog, DialogTrigger } from "./ui/dialog";
import { MermaidPopup } from "./MermaidPopup";
import {
  MermaidLoadError,
  mermaidAspectRatio,
  mermaidFrameDocument,
  renderMermaidSvg,
} from "@/lib/mermaid";

/** A small flowchart glyph: one node branching to two. */
function DiagramIcon() {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 16 16"
      className="h-3.5 w-3.5"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <rect x="5.5" y="1.5" width="5" height="3.5" rx="0.75" />
      <rect x="1" y="11" width="5" height="3.5" rx="0.75" />
      <rect x="10" y="11" width="5" height="3.5" rx="0.75" />
      <path d="M8 5v3M3.5 11V8h9v3" />
    </svg>
  );
}

function PreviewToggle({
  pressed,
  onToggle,
}: {
  pressed: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-pressed={pressed}
      aria-label="Preview Mermaid diagram"
      title={pressed ? "Show Mermaid source" : "Preview Mermaid diagram"}
      className={cn(
        "flex items-center gap-1 rounded px-2 py-0.5 text-[0.7rem] transition-colors hover:bg-accent hover:text-foreground",
        pressed ? "bg-accent text-foreground" : "text-muted-foreground",
      )}
    >
      <DiagramIcon />
      Diagram
    </button>
  );
}

export function MermaidBlock({ source }: { source: string }) {
  const reactId = React.useId();
  const renderId = React.useMemo(
    () => `mermaid-${reactId.replace(/[^a-zA-Z0-9_-]/g, "")}`,
    [reactId],
  );
  const [mode, setMode] = React.useState<"code" | "diagram">("code");
  const [svg, setSvg] = React.useState<string | null>(null);
  const [error, setError] = React.useState<{
    message: string;
    bundleMissing: boolean;
  } | null>(null);
  const [loading, setLoading] = React.useState(false);
  const [expanded, setExpanded] = React.useState(false);
  const requestRef = React.useRef(0);

  // Chat text can grow while streaming. Never leave a render of an earlier
  // partial definition visible, and ignore its async result if it arrives late.
  React.useLayoutEffect(() => {
    requestRef.current += 1;
    setMode("code");
    setSvg(null);
    setError(null);
    setLoading(false);
    setExpanded(false);
  }, [source]);

  React.useEffect(
    () => () => {
      requestRef.current += 1;
    },
    [],
  );

  const startRender = React.useCallback(() => {
    setLoading(true);
    setError(null);
    const request = ++requestRef.current;
    void renderMermaidSvg(renderId, source)
      .then((nextSvg) => {
        if (request === requestRef.current) setSvg(nextSvg);
      })
      .catch((err: unknown) => {
        if (request === requestRef.current) {
          setError({
            message:
              err instanceof Error ? err.message : "Could not render diagram.",
            bundleMissing: err instanceof MermaidLoadError,
          });
        }
      })
      .finally(() => {
        if (request === requestRef.current) setLoading(false);
      });
  }, [renderId, source]);

  const toggle = React.useCallback(() => {
    if (mode === "diagram") {
      setMode("code");
      return;
    }
    setMode("diagram");
    if (!svg && !loading) startRender();
  }, [loading, mode, startRender, svg]);

  if (mode === "code") {
    return (
      <CodeBlock
        code={source}
        language="mermaid"
        actions={<PreviewToggle pressed={false} onToggle={toggle} />}
      />
    );
  }

  const frame = svg ? mermaidFrameDocument(svg) : null;
  const ratio = svg ? mermaidAspectRatio(svg) : 16 / 9;

  return (
    <Dialog open={expanded} onOpenChange={setExpanded}>
      <div className="code-block bg-card" data-testid="mermaid-diagram">
        <div className="flex items-center justify-between border-b border-border bg-muted/60 px-3 py-1.5">
          <span className="text-[0.7rem] font-medium uppercase tracking-wide text-muted-foreground">
            Mermaid diagram
          </span>
          <div className="flex items-center gap-1">
            {svg && !loading && !error && (
              <DialogTrigger asChild>
                <button
                  type="button"
                  aria-label="Expand Mermaid diagram"
                  className="rounded px-2 py-0.5 text-[0.7rem] text-muted-foreground hover:bg-accent hover:text-foreground"
                >
                  Expand
                </button>
              </DialogTrigger>
            )}
            <PreviewToggle pressed onToggle={toggle} />
            <CodeCopyButton code={source} label="Copy Mermaid source" />
          </div>
        </div>

        {loading && (
          <div
            className="flex min-h-56 items-center justify-center text-sm text-muted-foreground"
            role="status"
          >
            Rendering diagram…
          </div>
        )}
        {!loading && error && (
          <div className="min-h-40 p-4 text-sm" role="alert">
            <p className="font-medium text-red-500">
              Could not render this Mermaid diagram.
            </p>
            <p className="mt-1 overflow-x-auto whitespace-pre font-mono text-xs text-muted-foreground">
              {error.message}
            </p>
            <div className="mt-3 flex gap-2">
              <button
                type="button"
                onClick={startRender}
                className="rounded border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-accent"
              >
                Retry
              </button>
              {error.bundleMissing && (
                <button
                  type="button"
                  onClick={() => window.location.reload()}
                  className="rounded border border-border px-2 py-1 text-xs text-foreground transition-colors hover:bg-accent"
                >
                  Reload page
                </button>
              )}
            </div>
          </div>
        )}
        {!loading && frame && (
          <iframe
            title="Mermaid diagram"
            srcDoc={frame}
            sandbox=""
            referrerPolicy="no-referrer"
            className="block min-h-56 w-full border-0 bg-white"
            style={{ aspectRatio: String(ratio), maxHeight: "70vh" }}
          />
        )}
      </div>
      {expanded && svg && <MermaidPopup svg={svg} source={source} />}
    </Dialog>
  );
}
